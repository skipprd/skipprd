use skippr_runtime_sdk::SkipprConfig;
use serde::Deserialize;
use serde_derive::Serialize;

pub const MAX_DEPTH_CAP: u32 = 200;
pub const MAX_QUERIES_PER_RUN_CAP: u32 = 100;
pub const MIN_QUERY_INTERVAL_MS_FLOOR: u64 = 3_000;
pub const DISCOVER_MAX_DEPTH: u32 = 10;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TargetEntry {
    pub app_id: String,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AppStoreEntity {
    Software,
    IPadSoftware,
    MacSoftware,
}

impl AppStoreEntity {
    pub fn as_str(self) -> &'static str {
        match self {
            AppStoreEntity::Software => "software",
            AppStoreEntity::IPadSoftware => "iPadSoftware",
            AppStoreEntity::MacSoftware => "macSoftware",
        }
    }

    pub fn parse(s: &str) -> Result<Self, std::io::Error> {
        match s.trim() {
            "software" => Ok(AppStoreEntity::Software),
            "iPadSoftware" => Ok(AppStoreEntity::IPadSoftware),
            "macSoftware" => Ok(AppStoreEntity::MacSoftware),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("entity must be software, iPadSoftware, or macSoftware (got {other})"),
            )),
        }
    }
}

impl Default for AppStoreEntity {
    fn default() -> Self {
        AppStoreEntity::Software
    }
}

#[derive(Debug, Clone, Deserialize, SkipprConfig, Serialize)]
pub struct DataSourceAppleAppStoreSerpPluginConfig {
    pub targets: Vec<TargetEntry>,
    pub keywords: Vec<String>,
    pub storefronts: Vec<String>,
    #[serde(default)]
    pub entity: AppStoreEntity,
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
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPair {
    pub keyword: String,
    pub storefront: String,
}

fn default_max_depth() -> u32 {
    50
}

fn default_min_query_interval_ms() -> u64 {
    3_000
}

fn default_max_queries_per_run() -> u32 {
    20
}

fn default_stop_after_first_target_match() -> bool {
    true
}

fn normalize_storefront(s: &str) -> String {
    s.trim().to_lowercase()
}

impl DataSourceAppleAppStoreSerpPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.targets.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "targets must include at least one app",
            ));
        }
        for target in &self.targets {
            if target.app_id.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "each target.app_id must be non-empty",
                ));
            }
        }
        if self.keywords.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "keywords must include at least one query",
            ));
        }
        if self.storefronts.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "storefronts must include at least one ISO country code",
            ));
        }
        for sf in &self.storefronts {
            let norm = normalize_storefront(sf);
            if norm.len() != 2 || !norm.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid storefront code: {sf}"),
                ));
            }
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
        if self.min_query_interval_ms < MIN_QUERY_INTERVAL_MS_FLOOR {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("min_query_interval_ms must be >= {MIN_QUERY_INTERVAL_MS_FLOOR}"),
            ));
        }
        Ok(())
    }

    pub fn normalized_storefronts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for sf in &self.storefronts {
            let norm = normalize_storefront(sf);
            if !norm.is_empty() && !out.contains(&norm) {
                out.push(norm);
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

    pub fn pairs_for_run(&self, discover: bool) -> Vec<QueryPair> {
        let keywords: Vec<String> = self
            .keywords
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        let keywords: Vec<String> = if discover {
            keywords.into_iter().take(1).collect()
        } else {
            keywords
        };

        let storefronts = self.normalized_storefronts();
        let storefronts: Vec<String> = if discover {
            storefronts.into_iter().take(1).collect()
        } else {
            storefronts
        };

        let mut pairs = Vec::new();
        for keyword in &keywords {
            for storefront in &storefronts {
                pairs.push(QueryPair {
                    keyword: keyword.clone(),
                    storefront: storefront.clone(),
                });
            }
        }

        let limit = if discover {
            1
        } else {
            self.max_queries_per_run.min(pairs.len() as u32) as usize
        };
        pairs.truncate(limit);
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> DataSourceAppleAppStoreSerpPluginConfig {
        DataSourceAppleAppStoreSerpPluginConfig {
            targets: vec![TargetEntry {
                app_id: "123456789".into(),
                bundle_id: Some("com.example.app".into()),
                aliases: vec!["987654321".into()],
            }],
            keywords: vec!["photo editor".into()],
            storefronts: vec!["us".into(), "gb".into()],
            entity: AppStoreEntity::Software,
            max_depth: 50,
            min_query_interval_ms: 3_000,
            max_queries_per_run: 20,
            stop_after_first_target_match: true,
            capture_results: false,
            force_refresh_today: false,
            user_agent: None,
        }
    }

    #[test]
    fn validates_volume_caps() {
        let mut cfg = sample_config();
        cfg.max_depth = 201;
        assert!(cfg.validate().is_err());
        cfg.max_depth = 50;
        cfg.min_query_interval_ms = 1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pairs_cartesian_product_capped() {
        let cfg = DataSourceAppleAppStoreSerpPluginConfig {
            keywords: vec!["a".into(), "b".into()],
            storefronts: vec!["us".into(), "gb".into(), "de".into()],
            max_queries_per_run: 3,
            ..sample_config()
        };
        let pairs = cfg.pairs_for_run(false);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].keyword, "a");
        assert_eq!(pairs[0].storefront, "us");
        assert_eq!(pairs[1].keyword, "a");
        assert_eq!(pairs[1].storefront, "gb");
        assert_eq!(pairs[2].keyword, "a");
        assert_eq!(pairs[2].storefront, "de");
    }

    #[test]
    fn discover_limits_to_one_pair() {
        let cfg = DataSourceAppleAppStoreSerpPluginConfig {
            keywords: vec!["a".into(), "b".into()],
            storefronts: vec!["us".into(), "gb".into()],
            ..sample_config()
        };
        assert_eq!(cfg.pairs_for_run(true).len(), 1);
    }

    #[test]
    fn unhappy_rejects_empty_storefronts() {
        let mut cfg = sample_config();
        cfg.storefronts.clear();
        assert!(cfg.validate().is_err());
    }
}
