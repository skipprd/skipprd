use serde::Deserialize;
use skippr_plugin_data_source_site_quality::config::{
    default_user_agent_for_profile, DeviceProfile, Viewport,
};
use skippr_plugin_data_source_site_quality::sampling::UrlMode;

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceSiteSecurityPluginConfig {
    pub site: String,
    #[serde(default = "default_url_mode")]
    pub url_mode: UrlMode,
    #[serde(default)]
    pub url_list: Vec<String>,
    #[serde(default = "default_max_pages_per_run")]
    pub max_pages_per_run: u32,
    #[serde(default = "default_max_crawl_depth")]
    pub max_crawl_depth: u32,
    #[serde(default)]
    pub crawl_seed_urls: Vec<String>,
    #[serde(default = "default_devices")]
    pub devices: Vec<DeviceProfile>,
    #[serde(default = "default_wait_until")]
    pub wait_until: String,
    #[serde(default = "default_navigation_timeout_ms")]
    pub navigation_timeout_ms: u32,
    #[serde(default = "default_pages_per_minute")]
    pub pages_per_minute: u32,
    #[serde(default = "default_worker_node_path")]
    pub worker_node_path: String,
    #[serde(default)]
    pub playwright_executable_path: Option<String>,
    #[serde(default = "default_respect_robots")]
    pub respect_robots: bool,
    #[serde(default = "default_max_third_party_scripts")]
    pub max_third_party_scripts: u32,
    #[serde(default = "default_import_lighthouse")]
    pub import_lighthouse_from_site_quality: bool,
}

fn default_url_mode() -> UrlMode {
    UrlMode::SiteCrawl
}

fn default_max_pages_per_run() -> u32 {
    30
}

fn default_max_crawl_depth() -> u32 {
    8
}

pub fn default_devices() -> Vec<DeviceProfile> {
    vec![DeviceProfile {
        profile: "desktop".into(),
        viewport: Viewport {
            width: 1350,
            height: 940,
        },
        user_agent: default_user_agent_for_profile("desktop"),
    }]
}

fn default_wait_until() -> String {
    "load".into()
}

fn default_navigation_timeout_ms() -> u32 {
    60_000
}

fn default_pages_per_minute() -> u32 {
    10
}

fn default_worker_node_path() -> String {
    "node".into()
}

fn default_respect_robots() -> bool {
    true
}

fn default_max_third_party_scripts() -> u32 {
    25
}

fn default_import_lighthouse() -> bool {
    true
}

impl DataSourceSiteSecurityPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.max_pages_per_run == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_pages_per_run must be > 0",
            ));
        }
        if self.devices.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "devices must not be empty",
            ));
        }
        Ok(())
    }
}
