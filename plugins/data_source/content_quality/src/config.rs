use serde::Deserialize;
use serde_derive::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataSourceContentQualityPluginConfig {
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
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_openai_enabled")]
    pub openai_enabled: bool,
    #[serde(default = "default_openai_max_blocks_per_page")]
    pub openai_max_blocks_per_page: u32,
    #[serde(default = "default_skip_unchanged_content")]
    pub skip_unchanged_content: bool,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub seed_urls: Vec<String>,
}

fn default_max_urls() -> u32 {
    500
}

fn default_max_depth() -> u32 {
    8
}

fn default_crawl_rate_per_second() -> f64 {
    1.5
}

fn default_respect_robots() -> bool {
    true
}

fn default_openai_model() -> String {
    "gpt-4.1-mini".into()
}

fn default_openai_enabled() -> bool {
    true
}

fn default_openai_max_blocks_per_page() -> u32 {
    24
}

fn default_skip_unchanged_content() -> bool {
    true
}

fn default_user_agent() -> String {
    "SkipprContentQuality/1.0".into()
}

impl DataSourceContentQualityPluginConfig {
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

    pub fn openai_active(&self) -> bool {
        self.openai_enabled
            && (std::env::var("OPENAI_API_KEY")
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
                || std::env::var("SKIPPR_CONTENT_QUALITY_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false)
                || std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false))
    }
}
