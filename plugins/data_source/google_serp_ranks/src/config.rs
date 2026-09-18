use skippr_runtime_sdk::SkipprConfig;
use serde::Deserialize;
use serde_derive::Serialize;

use crate::domain::normalize_domain;

pub const MAX_DEPTH_CAP: u32 = 100;
pub const MAX_QUERIES_PER_RUN_CAP: u32 = 100;
pub const MIN_QUERY_INTERVAL_MS_FLOOR: u64 = 5_000;
/// Hub post-process runs many allintitle queries inside the 15m Lambda budget.
pub const ALLINTITLE_MIN_QUERY_INTERVAL_MS_FLOOR: u64 = 2_000;
pub const DISCOVER_MAX_DEPTH: u32 = 10;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TargetEntry {
    pub site: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SerpDevice {
    Desktop,
    Mobile,
}

impl SerpDevice {
    pub fn as_str(self) -> &'static str {
        match self {
            SerpDevice::Desktop => "desktop",
            SerpDevice::Mobile => "mobile",
        }
    }
}

impl Default for SerpDevice {
    fn default() -> Self {
        SerpDevice::Desktop
    }
}

#[derive(Debug, Clone, Deserialize, SkipprConfig, Serialize)]
pub struct DataSourceGoogleSerpRanksPluginConfig {
    pub targets: Vec<TargetEntry>,
    pub keywords: Vec<String>,
    #[serde(default = "default_country")]
    pub country: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub device: SerpDevice,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_min_query_interval_ms")]
    pub min_query_interval_ms: u64,
    #[serde(default = "default_max_queries_per_run")]
    pub max_queries_per_run: u32,
    #[serde(default = "default_stop_after_first_target_match")]
    pub stop_after_first_target_match: bool,
    #[serde(default)]
    pub capture_results: bool,
    #[serde(default)]
    pub force_refresh_today: bool,
    #[serde(default = "default_navigation_timeout_ms")]
    pub navigation_timeout_ms: u32,
    #[serde(default = "default_worker_node_path")]
    pub worker_node_path: String,
    #[serde(default)]
    pub playwright_executable_path: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Bright Data SERP API zone (default `serp_api1`; override with `BRIGHTDATA_ZONE`).
    #[serde(default)]
    pub brightdata_zone: Option<String>,
    /// Bright Data API base URL (default `https://api.brightdata.com`).
    #[serde(default)]
    pub brightdata_api_base: Option<String>,
    /// Fetch `allintitle:{keyword}` counts via Bright Data (keyword hub KGR).
    #[serde(default)]
    pub include_allintitle: bool,
    /// Keywords for allintitle fetches; defaults to `keywords` when empty.
    #[serde(default)]
    pub allintitle_keywords: Vec<String>,
    /// When true, skip organic rank fetches and only emit allintitle_daily rows.
    #[serde(default)]
    pub allintitle_only: bool,
    /// Cap allintitle queries per run (defaults to `max_queries_per_run`).
    #[serde(default)]
    pub max_allintitle_queries_per_run: Option<u32>,
}

fn default_country() -> String {
    "uk".into()
}

fn default_language() -> String {
    "en".into()
}

fn default_max_depth() -> u32 {
    30
}

fn default_min_query_interval_ms() -> u64 {
    30_000
}

fn default_max_queries_per_run() -> u32 {
    10
}

fn default_stop_after_first_target_match() -> bool {
    true
}

fn default_navigation_timeout_ms() -> u32 {
    45_000
}

fn default_worker_node_path() -> String {
    "node".into()
}

impl DataSourceGoogleSerpRanksPluginConfig {
    fn min_query_interval_ms_floor(&self) -> u64 {
        if self.allintitle_only {
            ALLINTITLE_MIN_QUERY_INTERVAL_MS_FLOOR
        } else {
            MIN_QUERY_INTERVAL_MS_FLOOR
        }
    }

    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.targets.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "targets must include at least one site",
            ));
        }
        for target in &self.targets {
            if normalize_domain(&target.site).is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "each target.site must be a non-empty domain",
                ));
            }
        }
        if self.keywords.is_empty() && !self.allintitle_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "keywords must include at least one query",
            ));
        }
        if self.allintitle_only && self.allintitle_keywords_for_run().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "allintitle_keywords must include at least one query when allintitle_only is true",
            ));
        }
        if self.max_depth == 0 || self.max_depth > MAX_DEPTH_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("max_depth must be between 1 and {MAX_DEPTH_CAP}"),
            ));
        }
        if self.max_queries_per_run == 0 || self.max_queries_per_run > MAX_QUERIES_PER_RUN_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("max_queries_per_run must be between 1 and {MAX_QUERIES_PER_RUN_CAP}"),
            ));
        }
        let interval_floor = self.min_query_interval_ms_floor();
        if self.min_query_interval_ms < interval_floor {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("min_query_interval_ms must be >= {interval_floor}"),
            ));
        }
        if self.country.trim().is_empty() || self.language.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "country and language are required",
            ));
        }
        Ok(())
    }

    /// Flat list of domains to match (primary site + aliases), normalized.
    pub fn match_domains(&self) -> Vec<String> {
        let mut out = Vec::new();
        for target in &self.targets {
            let primary = normalize_domain(&target.site);
            if !primary.is_empty() && !out.contains(&primary) {
                out.push(primary);
            }
            for alias in &target.aliases {
                let domain = normalize_domain(alias);
                if !domain.is_empty() && !out.contains(&domain) {
                    out.push(domain);
                }
            }
        }
        out
    }

    pub fn effective_max_depth(&self, discover: bool) -> u32 {
        if discover {
            DISCOVER_MAX_DEPTH.min(self.max_depth)
        } else {
            self.max_depth
        }
    }

    pub fn queries_for_run(&self, discover: bool) -> Vec<String> {
        let mut keywords: Vec<String> = self
            .keywords
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if discover {
            keywords.truncate(1);
        }
        let limit = if discover {
            1
        } else {
            self.max_queries_per_run.min(keywords.len() as u32) as usize
        };
        keywords.truncate(limit);
        keywords
    }

    pub fn allintitle_keywords_for_run(&self) -> Vec<String> {
        let source = if self.allintitle_keywords.is_empty() {
            &self.keywords
        } else {
            &self.allintitle_keywords
        };
        let mut keywords: Vec<String> = source
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        let limit = self
            .max_allintitle_queries_per_run
            .unwrap_or(self.max_queries_per_run)
            .min(keywords.len() as u32) as usize;
        keywords.truncate(limit);
        keywords
    }

    pub fn primary_site(&self) -> String {
        self.targets
            .first()
            .map(|t| normalize_domain(&t.site))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> DataSourceGoogleSerpRanksPluginConfig {
        DataSourceGoogleSerpRanksPluginConfig {
            targets: vec![TargetEntry {
                site: "example.com".into(),
                aliases: vec!["www.example.com".into()],
            }],
            keywords: vec!["test query".into()],
            country: "uk".into(),
            language: "en".into(),
            device: SerpDevice::Desktop,
            max_depth: 30,
            min_query_interval_ms: 30_000,
            max_queries_per_run: 10,
            stop_after_first_target_match: true,
            capture_results: false,
            force_refresh_today: false,
            navigation_timeout_ms: 45_000,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            user_agent: None,
            brightdata_zone: Some("serp_api1".into()),
            brightdata_api_base: None,
            include_allintitle: false,
            allintitle_keywords: vec![],
            allintitle_only: false,
            max_allintitle_queries_per_run: None,
        }
    }

    #[test]
    fn validates_volume_caps() {
        let mut cfg = sample_config();
        cfg.max_depth = 101;
        assert!(cfg.validate().is_err());
        cfg.max_depth = 30;
        cfg.min_query_interval_ms = 1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn discover_limits_keywords() {
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            keywords: vec!["a".into(), "b".into(), "c".into()],
            ..sample_config()
        };
        assert_eq!(cfg.queries_for_run(true).len(), 1);
    }

    #[test]
    fn match_domains_dedupes_aliases() {
        let cfg = sample_config();
        let domains = cfg.match_domains();
        assert_eq!(domains, vec!["example.com".to_string()]);
    }

    #[test]
    fn unhappy_rejects_empty_targets() {
        let mut cfg = sample_config();
        cfg.targets.clear();
        let err = cfg.validate().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn unhappy_rejects_empty_keywords() {
        let mut cfg = sample_config();
        cfg.keywords.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unhappy_rejects_empty_target_site() {
        let mut cfg = sample_config();
        cfg.targets[0].site = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unhappy_rejects_max_depth_zero() {
        let mut cfg = sample_config();
        cfg.max_depth = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unhappy_rejects_max_queries_zero() {
        let mut cfg = sample_config();
        cfg.max_queries_per_run = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unhappy_rejects_empty_country() {
        let mut cfg = sample_config();
        cfg.country = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn max_queries_caps_in_normal_sync() {
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            keywords: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            max_queries_per_run: 2,
            ..sample_config()
        };
        assert_eq!(cfg.queries_for_run(false), vec!["a", "b"]);
    }

    #[test]
    fn effective_max_depth_capped_in_discover() {
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            max_depth: 50,
            ..sample_config()
        };
        assert_eq!(cfg.effective_max_depth(true), DISCOVER_MAX_DEPTH);
        assert_eq!(cfg.effective_max_depth(false), 50);
    }

    #[test]
    fn allintitle_only_accepts_hub_throttle_interval() {
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            keywords: vec![],
            allintitle_only: true,
            allintitle_keywords: vec!["kw".into()],
            min_query_interval_ms: 3_000,
            ..sample_config()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn allintitle_only_rejects_interval_below_hub_floor() {
        let mut cfg = DataSourceGoogleSerpRanksPluginConfig {
            keywords: vec![],
            allintitle_only: true,
            allintitle_keywords: vec!["kw".into()],
            ..sample_config()
        };
        cfg.min_query_interval_ms = 1_000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn max_allintitle_queries_caps_keyword_batch() {
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            allintitle_keywords: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            max_queries_per_run: 1,
            max_allintitle_queries_per_run: Some(3),
            ..sample_config()
        };
        assert_eq!(
            cfg.allintitle_keywords_for_run(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}
