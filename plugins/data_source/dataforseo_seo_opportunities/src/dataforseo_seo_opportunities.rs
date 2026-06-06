use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::{info, warn};

use crate::ai_citation::{ai_citation_score_from_features, detect_ai_citation_opportunities};
use crate::client::DataForSeoClient;
use crate::cluster::cluster_keywords_by_serp_overlap;
use crate::config::{DataForSeoSeoOpportunitiesPluginConfig, StreamKind};
use crate::content_brief::build_content_brief;
use crate::parse_allintitle::parse_allintitle_result;
use crate::parse_competitor::{
    parse_ranked_keyword_items, parse_sitemap_urls, CompetitorKeywordContext,
};
use crate::parse_keyword::{parse_keyword_suggestion_items, parse_seed_rows, KeywordParseContext};
use crate::parse_serp::{compute_weak_spots, parse_serp_items, SerpParseContext};
use crate::scoring::{compute_opportunity_score, OpportunityInputs};
use crate::streams::{
    NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY, NAMESPACE_ALLINTITLE_DAILY,
    NAMESPACE_COMPETITOR_KEYWORD_DAILY, NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY,
    NAMESPACE_CONTENT_BRIEF_DAILY, NAMESPACE_KEYWORD_CLUSTER_DAILY, NAMESPACE_KEYWORD_METRIC_DAILY,
    NAMESPACE_KEYWORD_SUGGESTION_DAILY, NAMESPACE_OPPORTUNITY_SCORE_DAILY,
    NAMESPACE_RANK_TRACKING_DAILY, NAMESPACE_SEED_KEYWORD_DAILY, NAMESPACE_SERP_FEATURE_DAILY,
    NAMESPACE_SERP_RESULT_DAILY, NAMESPACE_SITE_RUN_DAILY, NAMESPACE_WEAK_SPOT_DAILY,
};
use crate::target::normalize_site;

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
    seed_count: u32,
    keyword_count: u32,
    serp_count: u32,
    rows_by_stream: HashMap<String, u32>,
}

impl RunStats {
    fn add_rows(&mut self, stream: &str, count: u32) {
        *self.rows_by_stream.entry(stream.to_string()).or_insert(0) += count;
    }
}

#[derive(Debug, Default, Clone)]
struct KeywordRunState {
    pub search_volume: u64,
    pub keyword_difficulty: Option<u32>,
    pub cpc: Option<f64>,
    pub is_question: bool,
    pub intent: Option<String>,
    pub weakness_score: f64,
    pub forum_count: u32,
    pub ugc_count: u32,
    pub low_authority_count: u32,
    pub kgr: Option<f64>,
    pub allintitle_count: Option<u64>,
    pub own_rank: Option<u32>,
    pub serp_features: Vec<Value>,
    pub serp_results: Vec<Value>,
}

pub struct DataForSeoSeoOpportunitiesPlugin {
    config: DataForSeoSeoOpportunitiesPluginConfig,
    client: DataForSeoClient,
    site: String,
    own_domain: String,
    competitor_domains: Vec<String>,
}

impl DataForSeoSeoOpportunitiesPlugin {
    pub fn new(config: DataForSeoSeoOpportunitiesPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let (login, password) = if std::env::var(crate::client::FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            ("fixture".into(), "fixture".into())
        } else {
            config.resolve_credentials()?
        };
        let site = config.site_label()?;
        let own_domain = site.clone();
        let competitor_domains = config
            .competitors
            .iter()
            .filter_map(|c| normalize_site(&c.domain).ok())
            .collect();
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
            own_domain,
            competitor_domains,
        })
    }

    fn run_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn device_str(&self) -> &'static str {
        self.config.device.as_str()
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let run_date = FieldPath::single("run_date");
        let site = FieldPath::single("site");
        let partition_key = vec![run_date.clone()];
        let keyword = FieldPath::single("keyword");

        let (primary_key, description) = match namespace {
            NAMESPACE_SITE_RUN_DAILY => (
                vec![site.clone(), run_date.clone()],
                "DataForSEO SEO opportunities daily run rollup",
            ),
            NAMESPACE_SEED_KEYWORD_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("seed_keyword"),
                    run_date.clone(),
                ],
                "Seed keywords for SEO opportunity analysis",
            ),
            NAMESPACE_KEYWORD_SUGGESTION_DAILY => (
                vec![
                    site.clone(),
                    keyword.clone(),
                    FieldPath::single("seed_keyword"),
                    run_date.clone(),
                ],
                "Generated keyword suggestions",
            ),
            NAMESPACE_KEYWORD_METRIC_DAILY => (
                vec![site.clone(), keyword.clone(), run_date.clone()],
                "Keyword metrics snapshot",
            ),
            NAMESPACE_SERP_RESULT_DAILY => (
                vec![
                    site.clone(),
                    keyword.clone(),
                    FieldPath::single("rank_absolute"),
                    FieldPath::single("url"),
                    run_date.clone(),
                ],
                "SERP organic results",
            ),
            NAMESPACE_SERP_FEATURE_DAILY => (
                vec![
                    site.clone(),
                    keyword.clone(),
                    FieldPath::single("feature_type"),
                    FieldPath::single("question_text"),
                    run_date.clone(),
                ],
                "SERP feature snapshot",
            ),
            NAMESPACE_WEAK_SPOT_DAILY => (
                vec![
                    site.clone(),
                    keyword.clone(),
                    FieldPath::single("weakness_type"),
                    run_date.clone(),
                ],
                "SERP weakness analysis",
            ),
            NAMESPACE_KEYWORD_CLUSTER_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("cluster_id"),
                    keyword.clone(),
                    run_date.clone(),
                ],
                "Keyword clusters by SERP overlap",
            ),
            NAMESPACE_COMPETITOR_KEYWORD_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("competitor_name"),
                    keyword.clone(),
                    run_date.clone(),
                ],
                "Competitor ranking keywords",
            ),
            NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("competitor_name"),
                    FieldPath::single("url"),
                    run_date.clone(),
                ],
                "Competitor sitemap URLs",
            ),
            NAMESPACE_ALLINTITLE_DAILY => (
                vec![site.clone(), keyword.clone(), run_date.clone()],
                "Allintitle and KGR metrics",
            ),
            NAMESPACE_RANK_TRACKING_DAILY => (
                vec![
                    site.clone(),
                    keyword.clone(),
                    FieldPath::single("target_domain"),
                    run_date.clone(),
                ],
                "Keyword rank tracking snapshot",
            ),
            NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY => (
                vec![site.clone(), FieldPath::single("query"), run_date.clone()],
                "AI citation opportunities",
            ),
            NAMESPACE_CONTENT_BRIEF_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("brief_id"),
                    run_date.clone(),
                ],
                "Generated content briefs",
            ),
            NAMESPACE_OPPORTUNITY_SCORE_DAILY => (
                vec![site.clone(), keyword.clone(), run_date.clone()],
                "Keyword opportunity scores",
            ),
            _ => (
                vec![site.clone(), run_date.clone()],
                "DataForSEO SEO opportunities namespace",
            ),
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
                source_uri: format!("dataforseo-seo-opportunities://{}", self.site),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn site_run_row(&self, run_date: &str, stats: &RunStats, duration_ms: u64) -> Value {
        json!({
            "site": self.site,
            "run_date": run_date,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "device": self.device_str(),
            "seed_count": stats.seed_count,
            "keyword_count": stats.keyword_count,
            "serp_count": stats.serp_count,
            "api_cost_estimate": stats.total_api_cost_usd,
            "error_count": stats.tasks_error,
            "duration_ms": duration_ms,
            "tasks_ok": stats.tasks_ok,
            "rows_by_stream": stats.rows_by_stream,
        })
    }

    async fn fetch_keyword_suggestions(
        &self,
        seed: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<Vec<Value>, std::io::Error> {
        let task = json!({
            "keyword": seed,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "limit": self.config.effective_suggestion_limit(discover),
            "include_serp_info": true,
        });
        let response = self
            .client
            .post_keyword_suggestions_live(vec![task], "keyword_suggestions_live")
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok(Vec::new());
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            warn!(seed = %seed, message = %task_result.task_status_message, "keyword suggestions failed");
            return Ok(Vec::new());
        }
        stats.tasks_ok += 1;
        Ok(task_result.items)
    }

    async fn fetch_serp(
        &self,
        keyword: &str,
        depth: u32,
        stats: &mut RunStats,
        fixture: &str,
    ) -> Result<(Vec<Value>, Option<Value>), std::io::Error> {
        let task = json!({
            "keyword": keyword,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "device": self.device_str(),
            "depth": depth,
            "load_async_ai_overview": true,
        });
        let response = self
            .client
            .post_serp_organic_advanced_live(vec![task], fixture)
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok((Vec::new(), None));
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            return Ok((Vec::new(), None));
        }
        stats.tasks_ok += 1;
        Ok((task_result.items, task_result.result_body))
    }

    async fn fetch_allintitle(
        &self,
        keyword: &str,
        stats: &mut RunStats,
    ) -> Result<Option<Value>, std::io::Error> {
        let query = format!("allintitle:{keyword}");
        let task = json!({
            "keyword": query,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "device": self.device_str(),
            "depth": 1,
        });
        let response = self
            .client
            .post_serp_organic_advanced_live(vec![task], "allintitle_serp_live")
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok(None);
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            return Ok(None);
        }
        stats.tasks_ok += 1;
        Ok(task_result.result_body)
    }

    async fn fetch_competitor_keywords(
        &self,
        competitor_name: &str,
        domain: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<Vec<Value>, std::io::Error> {
        let task = json!({
            "target": domain,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "limit": if discover { 5 } else { 100 },
        });
        let response = self
            .client
            .post_ranked_keywords_live(vec![task], "ranked_keywords_live")
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok(Vec::new());
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            return Ok(Vec::new());
        }
        stats.tasks_ok += 1;
        let ctx = CompetitorKeywordContext {
            site: &self.site,
            run_date: &Self::run_date(),
            competitor_name,
            domain,
            own_domain: &self.own_domain,
            location_code: self.config.location_code,
            language_code: &self.config.language_code,
            device: self.device_str(),
        };
        Ok(parse_ranked_keyword_items(&task_result.items, &ctx))
    }

    async fn fetch_competitor_sitemap_urls(&self, domain: &str) -> Vec<String> {
        let client = reqwest::Client::new();
        let candidates = [
            format!("https://{domain}/sitemap.xml"),
            format!("https://www.{domain}/sitemap.xml"),
            format!("https://{domain}/sitemap_index.xml"),
        ];
        for url in candidates {
            let Ok(response) = client.get(&url).send().await else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            if let Ok(text) = response.text().await {
                let urls = extract_sitemap_urls(&text, domain);
                if !urls.is_empty() {
                    return urls;
                }
            }
        }
        Vec::new()
    }

    fn build_rank_tracking_rows(
        &self,
        run_date: &str,
        keyword: &str,
        serp_results: &[Value],
    ) -> Vec<Value> {
        let mut rank = None;
        let mut ranking_url = None;
        for row in serp_results {
            if row
                .get("is_own_domain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                rank = row
                    .get("rank_absolute")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                ranking_url = row.get("url").and_then(|v| v.as_str()).map(str::to_string);
                break;
            }
        }
        vec![json!({
            "site": self.site,
            "run_date": run_date,
            "keyword": keyword,
            "target_domain": self.own_domain,
            "rank": rank,
            "ranking_url": ranking_url,
            "previous_rank": null,
            "rank_delta": null,
            "top10_presence": rank.map(|r| r <= 10).unwrap_or(false),
            "serp_volatility": null,
            "featured_snippet_owned": false,
            "ai_feature_owned": false,
            "location_code": self.config.location_code,
            "language_code": self.config.language_code,
            "device": self.device_str(),
        })]
    }
}

fn extract_sitemap_urls(xml: &str, domain: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for fragment in xml.split("<loc>").skip(1) {
        let Some(end) = fragment.find("</loc>") else {
            continue;
        };
        let url = fragment[..end].trim();
        if !url.is_empty() && url.contains(domain) {
            urls.push(url.to_string());
        }
        if urls.len() >= 100 {
            break;
        }
    }
    urls
}

#[async_trait]
impl DataSource for DataForSeoSeoOpportunitiesPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let mut namespaces = vec![Self::namespace_contract(NAMESPACE_SITE_RUN_DAILY)];
        let add = |namespaces: &mut Vec<_>, ns: &str, enabled: bool| {
            if enabled {
                namespaces.push(Self::namespace_contract(ns));
            }
        };
        add(&mut namespaces, NAMESPACE_SEED_KEYWORD_DAILY, true);
        add(
            &mut namespaces,
            NAMESPACE_KEYWORD_SUGGESTION_DAILY,
            self.config.stream_enabled(StreamKind::KeywordSuggestions),
        );
        add(
            &mut namespaces,
            NAMESPACE_KEYWORD_METRIC_DAILY,
            self.config.stream_enabled(StreamKind::KeywordMetrics),
        );
        add(
            &mut namespaces,
            NAMESPACE_SERP_RESULT_DAILY,
            self.config.stream_enabled(StreamKind::SerpResults),
        );
        add(
            &mut namespaces,
            NAMESPACE_SERP_FEATURE_DAILY,
            self.config.stream_enabled(StreamKind::SerpFeatures),
        );
        add(
            &mut namespaces,
            NAMESPACE_WEAK_SPOT_DAILY,
            self.config.stream_enabled(StreamKind::WeakSpots),
        );
        add(
            &mut namespaces,
            NAMESPACE_KEYWORD_CLUSTER_DAILY,
            self.config.stream_enabled(StreamKind::KeywordClusters),
        );
        add(
            &mut namespaces,
            NAMESPACE_COMPETITOR_KEYWORD_DAILY,
            self.config.stream_enabled(StreamKind::CompetitorKeywords),
        );
        add(
            &mut namespaces,
            NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY,
            self.config.stream_enabled(StreamKind::CompetitorSitemaps),
        );
        add(
            &mut namespaces,
            NAMESPACE_ALLINTITLE_DAILY,
            self.config.stream_enabled(StreamKind::Allintitle),
        );
        add(
            &mut namespaces,
            NAMESPACE_RANK_TRACKING_DAILY,
            self.config.stream_enabled(StreamKind::RankTracking),
        );
        add(
            &mut namespaces,
            NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY,
            self.config
                .stream_enabled(StreamKind::AiCitationOpportunities),
        );
        add(
            &mut namespaces,
            NAMESPACE_CONTENT_BRIEF_DAILY,
            self.config.stream_enabled(StreamKind::ContentBriefs),
        );
        add(
            &mut namespaces,
            NAMESPACE_OPPORTUNITY_SCORE_DAILY,
            self.config.stream_enabled(StreamKind::OpportunityScores),
        );
        for contract in &namespaces {
            contract
                .validate()
                .expect("invalid SEO opportunities namespace contract");
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

        let started = std::time::Instant::now();
        let discover = runtime_is_discover_mode();
        let run_date = Self::run_date();
        if discover {
            info!(
                run_date = %run_date,
                "DataForSEO keyword research discover: one seed, suggestion preview"
            );
        }

        let mut stats = RunStats::default();
        let serp_depth = self.config.effective_serp_depth(discover);
        let seeds = self.config.seeds_for_run(discover);

        let mut seed_rows = Vec::new();
        let mut suggestion_rows = Vec::new();
        let mut metric_rows = Vec::new();
        let mut serp_result_rows = Vec::new();
        let mut serp_feature_rows = Vec::new();
        let mut weak_spot_rows = Vec::new();
        let mut allintitle_rows = Vec::new();
        let mut opportunity_rows = Vec::new();
        let mut ai_citation_rows = Vec::new();
        let mut rank_tracking_rows = Vec::new();
        let mut competitor_keyword_rows = Vec::new();
        let mut competitor_sitemap_rows = Vec::new();

        let mut keyword_states: HashMap<String, KeywordRunState> = HashMap::new();
        let mut keyword_domains: HashMap<String, HashSet<String>> = HashMap::new();
        let mut keyword_volumes: HashMap<String, u64> = HashMap::new();

        stats.seed_count = seeds.len() as u32;

        for (priority, (seed, seed_source)) in seeds.iter().enumerate() {
            let parse_ctx = KeywordParseContext {
                site: &self.site,
                run_date: &run_date,
                seed_keyword: seed,
                seed_source: *seed_source,
                location_code: self.config.location_code,
                language_code: &self.config.language_code,
                device: self.device_str(),
                suggestion_source: "labs_keyword_suggestions",
            };
            seed_rows.extend(parse_seed_rows(&parse_ctx, priority as u32 + 1));

            if !self.config.stream_enabled(StreamKind::KeywordSuggestions) {
                continue;
            }

            let items = self
                .fetch_keyword_suggestions(seed, discover, &mut stats)
                .await?;
            let (mut suggestions, mut metrics) = parse_keyword_suggestion_items(&items, &parse_ctx);

            let max_generated = if discover {
                crate::config::DISCOVER_SUGGESTION_LIMIT as usize
            } else {
                self.config.limits.max_generated_keywords
            };
            suggestions.truncate(max_generated);
            metrics.truncate(max_generated);

            for row in &suggestions {
                if let Some(keyword) = row.get("keyword").and_then(|v| v.as_str()) {
                    stats.keyword_count += 1;
                    let state = keyword_states.entry(keyword.to_string()).or_default();
                    state.search_volume = row
                        .get("search_volume")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    state.keyword_difficulty = row
                        .get("keyword_difficulty")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u32);
                    state.cpc = row.get("cpc").and_then(|v| v.as_f64());
                    state.is_question = row
                        .get("is_question")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    state.intent = row
                        .get("intent")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    keyword_volumes.insert(keyword.to_string(), state.search_volume);
                }
            }

            suggestion_rows.extend(suggestions);
            metric_rows.extend(metrics);
        }

        let keywords_for_analysis: Vec<String> = if discover {
            keyword_states.keys().take(1).cloned().collect()
        } else {
            keyword_states.keys().cloned().collect()
        };

        let serp_tracking_enabled = self
            .config
            .enabled_streams()
            .iter()
            .any(|s| s.is_serp_tracking());

        for keyword in &keywords_for_analysis {
            if serp_tracking_enabled {
                let (items, _result_body) = self
                    .fetch_serp(keyword, serp_depth, &mut stats, "serp_organic_live")
                    .await?;
                stats.serp_count += 1;
                let serp_ctx = SerpParseContext {
                    site: &self.site,
                    run_date: &run_date,
                    keyword,
                    own_domain: &self.own_domain,
                    competitor_domains: &self.competitor_domains,
                    location_code: self.config.location_code,
                    language_code: &self.config.language_code,
                    device: self.device_str(),
                };
                let parsed = parse_serp_items(&items, &serp_ctx);
                if self.config.stream_enabled(StreamKind::SerpResults) {
                    serp_result_rows.extend(parsed.results.clone());
                }
                if self.config.stream_enabled(StreamKind::SerpFeatures) {
                    serp_feature_rows.extend(parsed.features.clone());
                }
                if self.config.stream_enabled(StreamKind::WeakSpots) {
                    weak_spot_rows.extend(compute_weak_spots(
                        keyword,
                        &parsed.results,
                        &self.site,
                        &run_date,
                        &self.config.scoring,
                        self.config.location_code,
                        &self.config.language_code,
                        self.device_str(),
                    ));
                }

                let state = keyword_states.entry(keyword.to_string()).or_default();
                if let Some(weak) = weak_spot_rows
                    .iter()
                    .find(|r| r.get("keyword").and_then(|v| v.as_str()) == Some(keyword.as_str()))
                {
                    state.weakness_score = weak
                        .get("weakness_score")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    state.forum_count = weak
                        .get("forum_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    state.ugc_count =
                        weak.get("ugc_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    state.low_authority_count = weak
                        .get("low_authority_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                }
                state.serp_features = parsed.features;
                state.serp_results = parsed.results.clone();
                state.own_rank = parsed
                    .results
                    .iter()
                    .find(|r| {
                        r.get("is_own_domain")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .and_then(|r| r.get("rank_absolute").and_then(|v| v.as_u64()))
                    .map(|n| n as u32);

                let domains: HashSet<String> = parsed
                    .results
                    .iter()
                    .filter_map(|r| r.get("domain").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .collect();
                keyword_domains.insert(keyword.to_string(), domains);

                if self.config.stream_enabled(StreamKind::RankTracking)
                    || self.config.rank_track_keywords.iter().any(|k| k == keyword)
                {
                    rank_tracking_rows.extend(self.build_rank_tracking_rows(
                        &run_date,
                        keyword,
                        &parsed.results,
                    ));
                }

                if self
                    .config
                    .stream_enabled(StreamKind::AiCitationOpportunities)
                {
                    let features = keyword_states
                        .get(keyword)
                        .map(|s| s.serp_features.clone())
                        .unwrap_or_default();
                    let results = keyword_states
                        .get(keyword)
                        .map(|s| s.serp_results.clone())
                        .unwrap_or_default();
                    ai_citation_rows.extend(detect_ai_citation_opportunities(
                        &self.site,
                        &run_date,
                        &self.own_domain,
                        keyword,
                        &features,
                        &results,
                        self.config.location_code,
                        &self.config.language_code,
                        self.device_str(),
                    ));
                }
            }

            if self.config.stream_enabled(StreamKind::Allintitle)
                && self.config.scoring.include_allintitle
            {
                if let Some(result_body) = self.fetch_allintitle(keyword, &mut stats).await? {
                    let volume = keyword_states
                        .get(keyword)
                        .map(|s| s.search_volume)
                        .unwrap_or(0);
                    let row = parse_allintitle_result(
                        &result_body,
                        &self.site,
                        &run_date,
                        keyword,
                        volume,
                        self.config.location_code,
                        &self.config.language_code,
                        self.device_str(),
                    );
                    if let Some(state) = keyword_states.get_mut(keyword) {
                        state.kgr = row.get("kgr").and_then(|v| v.as_f64());
                        state.allintitle_count =
                            row.get("allintitle_count").and_then(|v| v.as_u64());
                    }
                    allintitle_rows.push(row);
                }
            }

            if self.config.stream_enabled(StreamKind::OpportunityScores) {
                if let Some(state) = keyword_states.get(keyword) {
                    let ai_score = ai_citation_score_from_features(
                        keyword,
                        &state.serp_features,
                        &self.own_domain,
                    );
                    let inputs = OpportunityInputs {
                        keyword: keyword.to_string(),
                        search_volume: state.search_volume,
                        keyword_difficulty: state.keyword_difficulty,
                        cpc: state.cpc,
                        weakness_score: state.weakness_score,
                        forum_count: state.forum_count,
                        ugc_count: state.ugc_count,
                        low_authority_count: state.low_authority_count,
                        is_question: state.is_question,
                        intent: state.intent.clone(),
                        kgr: state.kgr,
                        allintitle_count: state.allintitle_count,
                        own_rank: state.own_rank,
                        has_matching_page: state.own_rank.is_some(),
                        ai_citation_score: ai_score,
                    };
                    opportunity_rows.push(compute_opportunity_score(
                        &self.site,
                        &run_date,
                        &inputs,
                        &self.config.scoring,
                        self.config.location_code,
                        &self.config.language_code,
                        self.device_str(),
                    ));
                }
            }
        }

        let mut cluster_rows = Vec::new();
        if self.config.stream_enabled(StreamKind::KeywordClusters) && !keyword_domains.is_empty() {
            cluster_rows = cluster_keywords_by_serp_overlap(
                &self.site,
                &run_date,
                &keyword_domains,
                &keyword_volumes,
                self.config.location_code,
                &self.config.language_code,
                self.device_str(),
            );
        }

        let mut content_brief_rows = Vec::new();
        if self.config.stream_enabled(StreamKind::ContentBriefs) {
            for row in &opportunity_rows {
                let score = row
                    .get("opportunity_score")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if score < 40.0 {
                    continue;
                }
                let keyword = row
                    .get("keyword")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                content_brief_rows.push(build_content_brief(
                    &self.site,
                    &run_date,
                    "cluster_auto",
                    &keyword,
                    &[],
                    score,
                    self.config.location_code,
                    &self.config.language_code,
                    self.device_str(),
                ));
            }
        }

        if self.config.stream_enabled(StreamKind::CompetitorKeywords) {
            for competitor in &self.config.competitors {
                let domain = normalize_site(&competitor.domain).map_err(std::io::Error::other)?;
                let rows = self
                    .fetch_competitor_keywords(&competitor.name, &domain, discover, &mut stats)
                    .await?;
                competitor_keyword_rows.extend(rows);
            }
        }

        if self.config.stream_enabled(StreamKind::CompetitorSitemaps) && !discover {
            for competitor in &self.config.competitors {
                let domain = normalize_site(&competitor.domain).map_err(std::io::Error::other)?;
                let urls = self.fetch_competitor_sitemap_urls(&domain).await;
                if !urls.is_empty() {
                    competitor_sitemap_rows.extend(parse_sitemap_urls(
                        &self.site,
                        &run_date,
                        &competitor.name,
                        &format!("https://{domain}/sitemap.xml"),
                        &urls,
                        self.config.location_code,
                        &self.config.language_code,
                        self.device_str(),
                    ));
                }
            }
        }

        let submit = |stream: &str, ns: &str, rows: Vec<Value>, stats: &mut RunStats| {
            if rows.is_empty() {
                return Ok(());
            }
            let count = rows.len() as u32;
            stats.add_rows(stream, count);
            self.submit_rows(ctx.as_ref(), ns, &run_date, rows)
        };

        submit(
            "seed_keywords",
            NAMESPACE_SEED_KEYWORD_DAILY,
            seed_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::KeywordSuggestions.as_str(),
            NAMESPACE_KEYWORD_SUGGESTION_DAILY,
            suggestion_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::KeywordMetrics.as_str(),
            NAMESPACE_KEYWORD_METRIC_DAILY,
            metric_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::SerpResults.as_str(),
            NAMESPACE_SERP_RESULT_DAILY,
            serp_result_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::SerpFeatures.as_str(),
            NAMESPACE_SERP_FEATURE_DAILY,
            serp_feature_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::WeakSpots.as_str(),
            NAMESPACE_WEAK_SPOT_DAILY,
            weak_spot_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::Allintitle.as_str(),
            NAMESPACE_ALLINTITLE_DAILY,
            allintitle_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::OpportunityScores.as_str(),
            NAMESPACE_OPPORTUNITY_SCORE_DAILY,
            opportunity_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::KeywordClusters.as_str(),
            NAMESPACE_KEYWORD_CLUSTER_DAILY,
            cluster_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::CompetitorKeywords.as_str(),
            NAMESPACE_COMPETITOR_KEYWORD_DAILY,
            competitor_keyword_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::CompetitorSitemaps.as_str(),
            NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY,
            competitor_sitemap_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::RankTracking.as_str(),
            NAMESPACE_RANK_TRACKING_DAILY,
            rank_tracking_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::AiCitationOpportunities.as_str(),
            NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY,
            ai_citation_rows,
            &mut stats,
        )?;
        submit(
            StreamKind::ContentBriefs.as_str(),
            NAMESPACE_CONTENT_BRIEF_DAILY,
            content_brief_rows,
            &mut stats,
        )?;

        let duration_ms = started.elapsed().as_millis() as u64;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![self.site_run_row(&run_date, &stats, duration_ms)],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::client::FIXTURE_ENV;
    use crate::config::{CompetitorEntry, RunMode, DISCOVER_SUGGESTION_LIMIT};
    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{
        OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::{
        RuntimeOffsetMaterializationHint, SKIPPR_RUNTIME_EXECUTION_MODE_ENV,
    };
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn default_fixture_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures")
    }

    fn test_mvp_config() -> DataForSeoSeoOpportunitiesPluginConfig {
        DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["meal planning app".into()],
            run_mode: RunMode::Mvp,
            limits: crate::config::LimitsConfig {
                max_generated_keywords: 3,
                serp_depth: 10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn test_full_config_with_competitors() -> DataForSeoSeoOpportunitiesPluginConfig {
        DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["meal planning app".into()],
            run_mode: RunMode::Full,
            competitors: vec![CompetitorEntry {
                name: "CompetitorA".into(),
                domain: "competitor-a.com".into(),
            }],
            limits: crate::config::LimitsConfig {
                max_generated_keywords: 3,
                serp_depth: 10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn set_fixture_dir(dir: &str) {
        std::env::set_var(FIXTURE_ENV, dir);
    }

    fn clear_fixture_env() {
        std::env::remove_var(FIXTURE_ENV);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    #[derive(Default)]
    struct RecordingSyncContext {
        checkpoint_stores: Mutex<Vec<String>>,
        checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
        namespaces: Mutex<Vec<String>>,
        payloads: Mutex<HashMap<String, Vec<String>>>,
    }

    impl RecordingSyncContext {
        fn namespace_set(&self) -> HashSet<String> {
            self.namespaces.lock().unwrap().iter().cloned().collect()
        }

        fn payload_line_count(&self, namespace: &str) -> usize {
            self.payloads
                .lock()
                .unwrap()
                .get(namespace)
                .map(|lines| lines.len())
                .unwrap_or(0)
        }

        fn first_payload_line(&self, namespace: &str) -> Option<Value> {
            self.payloads
                .lock()
                .unwrap()
                .get(namespace)
                .and_then(|lines| lines.first())
                .and_then(|line| serde_json::from_str(line).ok())
        }
    }

    impl SourceSyncContext for RecordingSyncContext {
        fn submit_payload_tasks(
            &self,
            tasks: Vec<SourcePayloadTask>,
        ) -> Result<ThroughputMetrics, std::io::Error> {
            for task in tasks {
                for batch in task.batches {
                    if let Some(ns) = batch.namespace {
                        self.namespaces.lock().unwrap().push(ns.clone());
                        if !batch.data.is_empty() {
                            self.payloads
                                .lock()
                                .unwrap()
                                .entry(ns)
                                .or_default()
                                .extend(batch.data.lines().map(str::to_string));
                        }
                    }
                }
            }
            Ok(ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: 0,
                queue_length: 0,
                optimal_chunk_size: 0,
            })
        }

        fn validate_offset_batch(
            &self,
            entries: &[OffsetValidationEntry],
        ) -> Result<Vec<bool>, std::io::Error> {
            Ok(vec![false; entries.len()])
        }

        fn relay_offset_hints(
            &self,
            _hints: Vec<RuntimeOffsetMaterializationHint>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
            self.checkpoint_stores.lock().unwrap().push(key.to_string());
            self.checkpoints
                .lock()
                .unwrap()
                .insert(key.to_string(), envelope.clone());
            Ok(())
        }

        fn load_checkpoint_envelope(&self, key: &str) -> Option<CheckpointEnvelope> {
            self.checkpoints.lock().unwrap().get(key).cloned()
        }
    }

    #[test]
    fn namespace_contracts_for_mvp() {
        let _lock = env_lock();
        set_fixture_dir(default_fixture_dir());
        let plugin = DataForSeoSeoOpportunitiesPlugin::new(test_mvp_config()).unwrap();
        let namespaces: HashSet<_> = plugin
            .source_namespace_contracts()
            .into_iter()
            .map(|c| c.namespace)
            .collect();
        assert!(namespaces.contains(&NAMESPACE_ALLINTITLE_DAILY.to_string()));
        assert!(namespaces.contains(&NAMESPACE_OPPORTUNITY_SCORE_DAILY.to_string()));
        assert!(!namespaces.contains(&NAMESPACE_SERP_RESULT_DAILY.to_string()));
        assert!(!namespaces.contains(&NAMESPACE_WEAK_SPOT_DAILY.to_string()));
        assert!(!namespaces.contains(&NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY.to_string()));
        assert_eq!(namespaces.len(), 6);
        clear_fixture_env();
    }

    #[test]
    fn namespace_contracts_for_full() {
        let _lock = env_lock();
        set_fixture_dir(default_fixture_dir());
        let plugin =
            DataForSeoSeoOpportunitiesPlugin::new(test_full_config_with_competitors()).unwrap();
        let namespaces: HashSet<_> = plugin
            .source_namespace_contracts()
            .into_iter()
            .map(|c| c.namespace)
            .collect();
        assert_eq!(namespaces.len(), 15);
        assert!(namespaces.contains(&NAMESPACE_COMPETITOR_KEYWORD_DAILY.to_string()));
        assert!(namespaces.contains(&NAMESPACE_ALLINTITLE_DAILY.to_string()));
        assert!(namespaces.contains(&NAMESPACE_CONTENT_BRIEF_DAILY.to_string()));
        clear_fixture_env();
    }

    #[test]
    fn extract_sitemap_urls_parses_loc_tags() {
        let xml = "<urlset><url><loc>https://example.com/page-a</loc></url></urlset>";
        let urls = extract_sitemap_urls(xml, "example.com");
        assert_eq!(urls, vec!["https://example.com/page-a"]);
    }

    #[test]
    fn new_fails_without_credentials_or_fixture() {
        let _lock = env_lock();
        clear_fixture_env();
        std::env::remove_var("DATAFORSEO_API_USER");
        std::env::remove_var("DATAFORSEO_API_PASS");
        std::env::remove_var("DATAFORSEO_LOGIN");
        std::env::remove_var("DATAFORSEO_PASSWORD");

        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["test".into()],
            ..Default::default()
        };
        let err = DataForSeoSeoOpportunitiesPlugin::new(cfg)
            .err()
            .expect("missing credentials should fail");
        assert!(err.to_string().contains("login"));
    }

    #[tokio::test]
    async fn mvp_fixture_sync_emits_core_namespaces() {
        let _lock = env_lock();
        set_fixture_dir(default_fixture_dir());
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(test_mvp_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("mvp sync");

        let emitted = ctx.namespace_set();
        for ns in [
            NAMESPACE_SEED_KEYWORD_DAILY,
            NAMESPACE_KEYWORD_SUGGESTION_DAILY,
            NAMESPACE_KEYWORD_METRIC_DAILY,
            NAMESPACE_ALLINTITLE_DAILY,
            NAMESPACE_OPPORTUNITY_SCORE_DAILY,
            NAMESPACE_SITE_RUN_DAILY,
        ] {
            assert!(emitted.contains(ns), "missing namespace {ns}");
        }
        assert!(ctx.payload_line_count(NAMESPACE_KEYWORD_SUGGESTION_DAILY) > 0);
        let site_run = ctx
            .first_payload_line(NAMESPACE_SITE_RUN_DAILY)
            .expect("site_run row");
        assert_eq!(site_run["error_count"], 0);
        assert!(site_run["keyword_count"].as_u64().unwrap_or(0) > 0);

        clear_fixture_env();
    }

    #[tokio::test]
    async fn full_fixture_sync_includes_competitor_keywords() {
        let _lock = env_lock();
        set_fixture_dir(default_fixture_dir());
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin =
            DataForSeoSeoOpportunitiesPlugin::new(test_full_config_with_competitors()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("full sync");

        assert!(ctx.payload_line_count(NAMESPACE_COMPETITOR_KEYWORD_DAILY) > 0);
        assert!(ctx.payload_line_count(NAMESPACE_ALLINTITLE_DAILY) > 0);
        assert!(ctx.payload_line_count(NAMESPACE_KEYWORD_CLUSTER_DAILY) > 0);

        clear_fixture_env();
    }

    #[tokio::test]
    async fn discover_sync_bounded_seeds_and_skips_sitemaps() {
        let _lock = env_lock();
        set_fixture_dir(default_fixture_dir());
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["meal planning app".into(), "second seed".into()],
            run_mode: RunMode::Full,
            competitors: vec![CompetitorEntry {
                name: "CompetitorA".into(),
                domain: "competitor-a.com".into(),
            }],
            limits: crate::config::LimitsConfig {
                max_generated_keywords: 100,
                serp_depth: 20,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("discover sync");

        assert_eq!(ctx.payload_line_count(NAMESPACE_SEED_KEYWORD_DAILY), 1);
        assert!(
            ctx.payload_line_count(NAMESPACE_KEYWORD_SUGGESTION_DAILY)
                <= DISCOVER_SUGGESTION_LIMIT as usize
        );
        assert!(!ctx
            .namespace_set()
            .contains(NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY));

        clear_fixture_env();
    }

    #[tokio::test]
    async fn sync_continues_after_keyword_suggestion_task_error() {
        let _lock = env_lock();
        set_fixture_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/error"));
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(test_mvp_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin
            .sync(ctx.clone())
            .await
            .expect("sync after task error");

        let site_run = ctx
            .first_payload_line(NAMESPACE_SITE_RUN_DAILY)
            .expect("site_run row");
        assert!(site_run["error_count"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(
            ctx.payload_line_count(NAMESPACE_KEYWORD_SUGGESTION_DAILY),
            0
        );
        assert_eq!(ctx.payload_line_count(NAMESPACE_SEED_KEYWORD_DAILY), 1);

        clear_fixture_env();
    }

    #[tokio::test]
    async fn sync_handles_empty_keyword_suggestions() {
        let _lock = env_lock();
        set_fixture_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/empty"));
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(test_mvp_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin
            .sync(ctx.clone())
            .await
            .expect("sync with empty items");

        assert_eq!(
            ctx.payload_line_count(NAMESPACE_KEYWORD_SUGGESTION_DAILY),
            0
        );
        let site_run = ctx
            .first_payload_line(NAMESPACE_SITE_RUN_DAILY)
            .expect("site_run row");
        assert_eq!(site_run["keyword_count"], 0);
        assert_eq!(site_run["error_count"], 0);

        clear_fixture_env();
    }
}
