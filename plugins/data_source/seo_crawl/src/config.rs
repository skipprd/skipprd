use skippr_runtime_sdk::SkipprConfig;
use serde::Deserialize;
use serde_derive::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, SkipprConfig, PartialEq)]
pub struct DataSourceSeoCrawlPluginConfig {
    pub site: String,
    #[serde(default = "default_max_urls")]
    pub max_urls: u32,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default)]
    pub render_js: bool,
    #[serde(default = "default_crawl_rate_per_second")]
    pub crawl_rate_per_second: f64,
    #[serde(default = "default_respect_robots")]
    pub respect_robots: bool,
    #[serde(default = "default_sitemap_probe_paths")]
    pub sitemap_probe_paths: Vec<String>,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    /// When true, optional LLM pass for structural / AIO convention signals (not content prose).
    #[serde(default)]
    pub openai_structure_enabled: bool,
    #[serde(default = "default_skip_unchanged_content")]
    pub skip_unchanged_content: bool,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Extra paths or absolute URLs to seed the crawl queue (useful for JS SPAs with no static links).
    #[serde(default)]
    pub seed_urls: Vec<String>,
    /// When non-empty, process only these URLs (discovery is external).
    #[serde(default)]
    pub url_list: Vec<String>,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

fn default_max_response_bytes() -> usize {
    2_097_152
}

fn default_max_urls() -> u32 {
    5000
}

fn default_max_depth() -> u32 {
    8
}

fn default_crawl_rate_per_second() -> f64 {
    2.0
}

fn default_respect_robots() -> bool {
    true
}

fn default_sitemap_probe_paths() -> Vec<String> {
    vec![
        "/sitemap.xml".into(),
        "/sitemap_index.xml".into(),
        "/sitemap-index.xml".into(),
    ]
}

fn default_openai_model() -> String {
    "gpt-4.1-mini".into()
}

fn default_skip_unchanged_content() -> bool {
    true
}

fn default_user_agent() -> String {
    "SkipprSeoCrawl/1.0".into()
}

impl DataSourceSeoCrawlPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.max_urls == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_urls must be >= 1",
            ));
        }
        Ok(())
    }

    pub fn openai_structure_active(&self) -> bool {
        self.openai_structure_enabled
            && (std::env::var("OPENAI_API_KEY")
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
                || std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false)
                || std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false))
    }
}
