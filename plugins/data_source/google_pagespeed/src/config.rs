use serde::Deserialize;
use serde_derive::Serialize;

use crate::sampling::UrlMode;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Mobile,
    Desktop,
}

impl Strategy {
    pub fn as_api_str(&self) -> &'static str {
        match self {
            Self::Mobile => "mobile",
            Self::Desktop => "desktop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mobile" => Some(Self::Mobile),
            "desktop" => Some(Self::Desktop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceGooglePageSpeedPluginConfig {
    pub site: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_url_mode")]
    pub url_mode: UrlMode,
    #[serde(default)]
    pub url_list: Vec<String>,
    #[serde(default = "default_max_urls")]
    pub max_urls: u32,
    #[serde(default = "default_strategies")]
    pub strategies: Vec<Strategy>,
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_max_requests_per_run")]
    pub max_requests_per_run: u32,
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    #[serde(default = "default_respect_robots")]
    pub respect_robots: bool,
    #[serde(default = "default_top_audits_per_page")]
    pub top_audits_per_page: u32,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u32,
}

fn default_url_mode() -> UrlMode {
    UrlMode::TldSample
}

fn default_max_urls() -> u32 {
    50
}

fn default_strategies() -> Vec<Strategy> {
    vec![Strategy::Mobile, Strategy::Desktop]
}

fn default_categories() -> Vec<String> {
    vec![
        "performance".into(),
        "accessibility".into(),
        "best-practices".into(),
        "seo".into(),
    ]
}

fn default_locale() -> String {
    "en_US".into()
}

fn default_max_requests_per_run() -> u32 {
    120
}

fn default_requests_per_minute() -> u32 {
    30
}

fn default_respect_robots() -> bool {
    true
}

fn default_top_audits_per_page() -> u32 {
    15
}

fn default_max_concurrent_requests() -> u32 {
    2
}

impl DataSourceGooglePageSpeedPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Google PageSpeed requires non-empty site URL",
            ));
        }
        if self.strategies.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Google PageSpeed requires at least one strategy (mobile, desktop)",
            ));
        }
        if self.categories.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Google PageSpeed requires at least one Lighthouse category",
            ));
        }
        if self.max_urls == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_urls must be at least 1",
            ));
        }
        Ok(())
    }

    pub fn resolve_api_key(&self) -> Result<String, std::io::Error> {
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
            return Ok(key.trim().to_string());
        }
        if let Ok(key) = std::env::var("PAGESPEED_API_KEY") {
            if !key.trim().is_empty() {
                return Ok(key.trim().to_string());
            }
        }
        if std::env::var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok("fixture".into());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Google PageSpeed requires api_key in config or PAGESPEED_API_KEY env",
        ))
    }
}
