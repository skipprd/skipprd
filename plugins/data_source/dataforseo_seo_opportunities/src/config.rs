use serde::Deserialize;
use serde_derive::Serialize;

use crate::target::normalize_site;

pub const DISCOVER_MAX_SEEDS: usize = 1;
pub const DISCOVER_SUGGESTION_LIMIT: u32 = 5;
pub const DISCOVER_SERP_DEPTH: u32 = 10;
pub const DEFAULT_SERP_DEPTH: u32 = 20;
pub const DEFAULT_MAX_SEED_KEYWORDS: usize = 200;
pub const DEFAULT_MAX_GENERATED_KEYWORDS: usize = 10_000;
pub const DEFAULT_MAX_COMPETITORS: usize = 20;
pub const DEFAULT_WEAK_DOMAIN_RANK_THRESHOLD: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Full,
    Mvp,
    DiscoverOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchEngine {
    #[default]
    Google,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    #[default]
    Desktop,
    Mobile,
}

impl Device {
    pub fn as_str(self) -> &'static str {
        match self {
            Device::Desktop => "desktop",
            Device::Mobile => "mobile",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    KeywordSuggestions,
    KeywordMetrics,
    SerpResults,
    SerpFeatures,
    WeakSpots,
    KeywordClusters,
    CompetitorKeywords,
    CompetitorSitemaps,
    Allintitle,
    RankTracking,
    AiCitationOpportunities,
    ContentBriefs,
    OpportunityScores,
}

impl StreamKind {
    /// Labs keyword expansion, MSV/KD, KGR — not SERP rank tracking (use `GoogleSerpRanks` / Bright Data).
    pub fn keyword_research_streams() -> &'static [StreamKind] {
        &[
            StreamKind::KeywordSuggestions,
            StreamKind::KeywordMetrics,
            StreamKind::Allintitle,
            StreamKind::OpportunityScores,
        ]
    }

    /// Live SERP snapshots and rank-style outputs from DataForSEO (opt-in only).
    ///
    /// **Deprecated for Up Foundry keyword hub:** use Bright Data `google_serp_ranks` and dbt
    /// `keyword_cluster_daily` instead of `serp_result_daily`, `weak_spot_daily`, and DFS clusters.
    pub fn serp_tracking_streams() -> &'static [StreamKind] {
        &[
            StreamKind::SerpResults,
            StreamKind::SerpFeatures,
            StreamKind::WeakSpots,
            StreamKind::RankTracking,
            StreamKind::KeywordClusters,
        ]
    }

    pub fn is_serp_tracking(self) -> bool {
        Self::serp_tracking_streams().contains(&self)
    }

    pub fn mvp_streams() -> &'static [StreamKind] {
        Self::keyword_research_streams()
    }

    pub fn all() -> &'static [StreamKind] {
        &[
            StreamKind::KeywordSuggestions,
            StreamKind::KeywordMetrics,
            StreamKind::SerpResults,
            StreamKind::SerpFeatures,
            StreamKind::WeakSpots,
            StreamKind::KeywordClusters,
            StreamKind::CompetitorKeywords,
            StreamKind::CompetitorSitemaps,
            StreamKind::Allintitle,
            StreamKind::RankTracking,
            StreamKind::AiCitationOpportunities,
            StreamKind::ContentBriefs,
            StreamKind::OpportunityScores,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::KeywordSuggestions => "keyword_suggestions",
            StreamKind::KeywordMetrics => "keyword_metrics",
            StreamKind::SerpResults => "serp_results",
            StreamKind::SerpFeatures => "serp_features",
            StreamKind::WeakSpots => "weak_spots",
            StreamKind::KeywordClusters => "keyword_clusters",
            StreamKind::CompetitorKeywords => "competitor_keywords",
            StreamKind::CompetitorSitemaps => "competitor_sitemaps",
            StreamKind::Allintitle => "allintitle",
            StreamKind::RankTracking => "rank_tracking",
            StreamKind::AiCitationOpportunities => "ai_citation_opportunities",
            StreamKind::ContentBriefs => "content_briefs",
            StreamKind::OpportunityScores => "opportunity_scores",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    #[default]
    Config,
    Gsc,
    Crawl,
    Competitor,
    Autocomplete,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompetitorEntry {
    pub name: String,
    pub domain: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_seed_keywords")]
    pub max_seed_keywords: usize,
    #[serde(default = "default_max_generated_keywords")]
    pub max_generated_keywords: usize,
    #[serde(default = "default_serp_depth")]
    pub serp_depth: u32,
    #[serde(default = "default_rank_track_depth")]
    pub rank_track_depth: u32,
    #[serde(default = "default_max_competitors")]
    pub max_competitors: usize,
}

fn default_max_seed_keywords() -> usize {
    DEFAULT_MAX_SEED_KEYWORDS
}

fn default_max_generated_keywords() -> usize {
    DEFAULT_MAX_GENERATED_KEYWORDS
}

fn default_serp_depth() -> u32 {
    DEFAULT_SERP_DEPTH
}

fn default_rank_track_depth() -> u32 {
    100
}

fn default_max_competitors() -> usize {
    DEFAULT_MAX_COMPETITORS
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScoringConfig {
    #[serde(default = "default_min_search_volume")]
    pub min_search_volume: u32,
    #[serde(default = "default_max_keyword_difficulty")]
    pub max_keyword_difficulty: u32,
    #[serde(default = "default_weak_domain_rank_threshold")]
    pub weak_domain_rank_threshold: u32,
    #[serde(default = "default_prefer_question_keywords")]
    pub prefer_question_keywords: bool,
    #[serde(default = "default_prefer_low_backlink_serps")]
    pub prefer_low_backlink_serps: bool,
    #[serde(default = "default_include_allintitle")]
    pub include_allintitle: bool,
    #[serde(default = "default_include_kgr")]
    pub include_kgr: bool,
}

fn default_min_search_volume() -> u32 {
    10
}

fn default_max_keyword_difficulty() -> u32 {
    50
}

fn default_weak_domain_rank_threshold() -> u32 {
    DEFAULT_WEAK_DOMAIN_RANK_THRESHOLD
}

fn default_prefer_question_keywords() -> bool {
    true
}

fn default_prefer_low_backlink_serps() -> bool {
    true
}

fn default_include_allintitle() -> bool {
    true
}

fn default_include_kgr() -> bool {
    true
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            min_search_volume: default_min_search_volume(),
            max_keyword_difficulty: default_max_keyword_difficulty(),
            weak_domain_rank_threshold: default_weak_domain_rank_threshold(),
            prefer_question_keywords: default_prefer_question_keywords(),
            prefer_low_backlink_serps: default_prefer_low_backlink_serps(),
            include_allintitle: default_include_allintitle(),
            include_kgr: default_include_kgr(),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_seed_keywords: default_max_seed_keywords(),
            max_generated_keywords: default_max_generated_keywords(),
            serp_depth: default_serp_depth(),
            rank_track_depth: default_rank_track_depth(),
            max_competitors: default_max_competitors(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataForSeoSeoOpportunitiesPluginConfig {
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    pub site: String,
    #[serde(default = "default_location_code")]
    pub location_code: u32,
    #[serde(default = "default_language_code")]
    pub language_code: String,
    #[serde(default)]
    pub device: Device,
    #[serde(default)]
    pub search_engine: SearchEngine,
    #[serde(default)]
    pub run_mode: RunMode,
    #[serde(default)]
    pub seed_keywords: Vec<String>,
    #[serde(default)]
    pub seed_queries_from_gsc: bool,
    #[serde(default)]
    pub gsc_source: Option<String>,
    #[serde(default)]
    pub seed_urls_from_crawl: bool,
    #[serde(default)]
    pub seo_crawl_source: Option<String>,
    #[serde(default)]
    pub competitors: Vec<CompetitorEntry>,
    #[serde(default)]
    pub rank_track_keywords: Vec<String>,
    #[serde(default)]
    pub streams: Vec<StreamKind>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub scoring: ScoringConfig,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_openai_enabled")]
    pub openai_enabled: bool,
    #[serde(default = "default_request_interval_ms")]
    pub request_interval_ms: u64,
    #[serde(default = "default_max_api_retries")]
    pub max_api_retries: u32,
}

fn default_location_code() -> u32 {
    2840
}

fn default_language_code() -> String {
    "en".into()
}

fn default_openai_model() -> String {
    "gpt-4.1-mini".into()
}

fn default_openai_enabled() -> bool {
    true
}

fn default_request_interval_ms() -> u64 {
    200
}

fn default_max_api_retries() -> u32 {
    8
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve login/password from environment (used by config and `skippr doctor`).
pub fn credentials_from_env() -> Option<(String, String)> {
    let login = env_nonempty("DATAFORSEO_API_USER").or_else(|| env_nonempty("DATAFORSEO_LOGIN"))?;
    let password =
        env_nonempty("DATAFORSEO_API_PASS").or_else(|| env_nonempty("DATAFORSEO_PASSWORD"))?;
    Some((login, password))
}

impl DataForSeoSeoOpportunitiesPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        normalize_site(&self.site).map_err(std::io::Error::other)?;
        if self.seed_keywords.is_empty() && self.run_mode != RunMode::DiscoverOnly {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seed_keywords must include at least one keyword",
            ));
        }
        if self.language_code.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "language_code is required",
            ));
        }
        if self.limits.max_seed_keywords == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "limits.max_seed_keywords must be >= 1",
            ));
        }
        if self.limits.serp_depth == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "limits.serp_depth must be >= 1",
            ));
        }
        if self.competitors.len() > self.limits.max_competitors {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "competitors has {} entries (max {})",
                    self.competitors.len(),
                    self.limits.max_competitors
                ),
            ));
        }
        let mut names = std::collections::HashSet::new();
        for competitor in &self.competitors {
            if competitor.name.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "competitor name is required",
                ));
            }
            if !names.insert(competitor.name.trim().to_string()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("duplicate competitor name '{}'", competitor.name),
                ));
            }
            normalize_site(&competitor.domain).map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    pub fn enabled_streams(&self) -> Vec<StreamKind> {
        if !self.streams.is_empty() {
            return self.streams.clone();
        }
        match self.run_mode {
            RunMode::Mvp => StreamKind::mvp_streams().to_vec(),
            RunMode::Full | RunMode::DiscoverOnly => StreamKind::all().to_vec(),
        }
    }

    pub fn stream_enabled(&self, kind: StreamKind) -> bool {
        self.enabled_streams().contains(&kind)
    }

    pub fn resolve_credentials(&self) -> Result<(String, String), std::io::Error> {
        let login = self
            .login
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| credentials_from_env().map(|(l, _)| l))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "DataForSEO login is required (config.login, DATAFORSEO_API_USER, or DATAFORSEO_LOGIN)",
                )
            })?;
        let password = self
            .password
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| credentials_from_env().map(|(_, p)| p))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "DataForSEO password is required (config.password, DATAFORSEO_API_PASS, or DATAFORSEO_PASSWORD)",
                )
            })?;
        Ok((login, password))
    }

    pub fn site_label(&self) -> Result<String, std::io::Error> {
        normalize_site(&self.site).map_err(std::io::Error::other)
    }

    pub fn effective_serp_depth(&self, discover: bool) -> u32 {
        if discover {
            DISCOVER_SERP_DEPTH
        } else {
            self.limits.serp_depth
        }
    }

    pub fn effective_suggestion_limit(&self, discover: bool) -> u32 {
        if discover {
            DISCOVER_SUGGESTION_LIMIT
        } else {
            100
        }
    }

    pub fn seeds_for_run(&self, discover: bool) -> Vec<(String, SeedSource)> {
        let max = if discover {
            DISCOVER_MAX_SEEDS
        } else {
            self.limits.max_seed_keywords
        };
        self.seed_keywords
            .iter()
            .take(max)
            .map(|k| (k.trim().to_string(), SeedSource::Config))
            .filter(|(k, _)| !k.is_empty())
            .collect()
    }

    pub fn openai_active(&self) -> bool {
        self.openai_enabled
            && (std::env::var("OPENAI_API_KEY")
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
                || std::env::var("SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false)
                || std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mvp_default_streams_when_empty() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["test".into()],
            run_mode: RunMode::Mvp,
            ..Default::default()
        };
        let streams = cfg.enabled_streams();
        assert!(streams.contains(&StreamKind::KeywordMetrics));
        assert!(!streams.contains(&StreamKind::SerpResults));
        assert!(!streams.contains(&StreamKind::RankTracking));
        assert!(!streams.contains(&StreamKind::CompetitorSitemaps));
    }

    #[test]
    fn rejects_empty_seeds_in_full_mode() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn discover_only_allows_empty_seeds() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            run_mode: RunMode::DiscoverOnly,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_competitor_names() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["test".into()],
            competitors: vec![
                CompetitorEntry {
                    name: "Same".into(),
                    domain: "a.com".into(),
                },
                CompetitorEntry {
                    name: "Same".into(),
                    domain: "b.com".into(),
                },
            ],
            ..Default::default()
        };
        let err = cfg.validate().expect_err("duplicate names");
        assert!(err.to_string().contains("duplicate competitor name"));
    }

    #[test]
    fn rejects_too_many_competitors() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["test".into()],
            limits: LimitsConfig {
                max_competitors: 1,
                ..Default::default()
            },
            competitors: vec![
                CompetitorEntry {
                    name: "A".into(),
                    domain: "a.com".into(),
                },
                CompetitorEntry {
                    name: "B".into(),
                    domain: "b.com".into(),
                },
            ],
            ..Default::default()
        };
        let err = cfg.validate().expect_err("too many competitors");
        assert!(err.to_string().contains("competitors has 2 entries"));
    }

    #[test]
    fn rejects_empty_site() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "   ".into(),
            seed_keywords: vec!["test".into()],
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn resolve_credentials_from_config() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["test".into()],
            login: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        let (login, password) = cfg.resolve_credentials().unwrap();
        assert_eq!(login, "user");
        assert_eq!(password, "pass");
    }

    #[test]
    fn seeds_for_run_discover_caps_at_one() {
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            seed_keywords: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert_eq!(cfg.seeds_for_run(true).len(), DISCOVER_MAX_SEEDS);
        assert_eq!(cfg.seeds_for_run(false).len(), 3);
    }

    #[test]
    fn openai_active_with_fixture_dir() {
        std::env::set_var(crate::client::FIXTURE_ENV, "/tmp/fixture");
        let cfg = DataForSeoSeoOpportunitiesPluginConfig {
            site: "example.com".into(),
            openai_enabled: true,
            ..Default::default()
        };
        assert!(cfg.openai_active());
        std::env::remove_var(crate::client::FIXTURE_ENV);
    }
}

impl Default for DataForSeoSeoOpportunitiesPluginConfig {
    fn default() -> Self {
        Self {
            login: None,
            password: None,
            site: String::new(),
            location_code: default_location_code(),
            language_code: default_language_code(),
            device: Device::default(),
            search_engine: SearchEngine::default(),
            run_mode: RunMode::default(),
            seed_keywords: Vec::new(),
            seed_queries_from_gsc: false,
            gsc_source: None,
            seed_urls_from_crawl: false,
            seo_crawl_source: None,
            competitors: Vec::new(),
            rank_track_keywords: Vec::new(),
            streams: Vec::new(),
            limits: LimitsConfig::default(),
            scoring: ScoringConfig::default(),
            openai_model: default_openai_model(),
            openai_enabled: default_openai_enabled(),
            request_interval_ms: default_request_interval_ms(),
            max_api_retries: default_max_api_retries(),
        }
    }
}
