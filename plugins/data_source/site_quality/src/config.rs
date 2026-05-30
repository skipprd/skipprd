use serde::Deserialize;
use serde_derive::Serialize;

use crate::sampling::UrlMode;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceProfile {
    pub profile: String,
    pub viewport: Viewport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThrottleConfig {
    #[serde(default = "default_rtt_ms")]
    pub rtt_ms: u32,
    #[serde(default = "default_throughput_kbps")]
    pub throughput_kbps: f64,
    #[serde(default = "default_cpu_slowdown")]
    pub cpu_slowdown: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceSiteQualityPluginConfig {
    pub site: String,
    #[serde(default = "default_url_mode")]
    pub url_mode: UrlMode,
    #[serde(default)]
    pub url_list: Vec<String>,
    #[serde(default = "default_max_pages_per_run")]
    pub max_pages_per_run: u32,
    #[serde(default = "default_devices")]
    pub devices: Vec<DeviceProfile>,
    #[serde(default = "default_wait_until")]
    pub wait_until: String,
    #[serde(default = "default_navigation_timeout_ms")]
    pub navigation_timeout_ms: u32,
    #[serde(default = "default_lighthouse_enabled")]
    pub lighthouse_enabled: bool,
    #[serde(default = "default_lighthouse_categories")]
    pub lighthouse_categories: Vec<String>,
    #[serde(default = "default_axe_enabled")]
    pub axe_enabled: bool,
    #[serde(default = "default_axe_tags")]
    pub axe_tags: Vec<String>,
    #[serde(default)]
    pub throttle: ThrottleConfig,
    #[serde(default = "default_pages_per_minute")]
    pub pages_per_minute: u32,
    #[serde(default = "default_worker_node_path")]
    pub worker_node_path: String,
    #[serde(default)]
    pub playwright_executable_path: Option<String>,
    #[serde(default = "default_respect_robots")]
    pub respect_robots: bool,
    #[serde(default = "default_skip_heavy_when_unchanged")]
    pub skip_heavy_when_unchanged: bool,
}

fn default_url_mode() -> UrlMode {
    UrlMode::TldSample
}

fn default_max_pages_per_run() -> u32 {
    50
}

pub fn default_devices() -> Vec<DeviceProfile> {
    vec![
        DeviceProfile {
            profile: "mobile".into(),
            viewport: Viewport {
                width: 390,
                height: 844,
            },
        },
        DeviceProfile {
            profile: "desktop".into(),
            viewport: Viewport {
                width: 1350,
                height: 940,
            },
        },
    ]
}

fn default_wait_until() -> String {
    // SPAs often never reach networkidle; load is enough for lab metrics and axe.
    "load".into()
}

fn default_navigation_timeout_ms() -> u32 {
    45_000
}

fn default_lighthouse_enabled() -> bool {
    true
}

fn default_lighthouse_categories() -> Vec<String> {
    vec![
        "performance".into(),
        "accessibility".into(),
        "best-practices".into(),
        "seo".into(),
    ]
}

fn default_axe_enabled() -> bool {
    true
}

fn default_axe_tags() -> Vec<String> {
    vec!["wcag2a".into(), "wcag2aa".into()]
}

fn default_rtt_ms() -> u32 {
    150
}

fn default_throughput_kbps() -> f64 {
    1638.4
}

fn default_cpu_slowdown() -> u32 {
    4
}

fn default_pages_per_minute() -> u32 {
    6
}

fn default_worker_node_path() -> String {
    "node".into()
}

fn default_respect_robots() -> bool {
    true
}

fn default_skip_heavy_when_unchanged() -> bool {
    true
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            rtt_ms: default_rtt_ms(),
            throughput_kbps: default_throughput_kbps(),
            cpu_slowdown: default_cpu_slowdown(),
        }
    }
}

impl DataSourceSiteQualityPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.devices.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "devices must include at least one profile",
            ));
        }
        if self.url_mode == UrlMode::UrlList && self.url_list.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "url_list is required when url_mode is url_list",
            ));
        }
        if self.max_pages_per_run == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_pages_per_run must be >= 1",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampling::UrlMode;

    #[test]
    fn url_list_requires_urls() {
        let cfg = DataSourceSiteQualityPluginConfig {
            site: "https://example.com".into(),
            url_mode: UrlMode::UrlList,
            url_list: vec![],
            max_pages_per_run: 10,
            devices: default_devices(),
            wait_until: "load".into(),
            navigation_timeout_ms: 5000,
            lighthouse_enabled: false,
            lighthouse_categories: vec![],
            axe_enabled: false,
            axe_tags: vec![],
            throttle: Default::default(),
            pages_per_minute: 6,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            respect_robots: true,
            skip_heavy_when_unchanged: true,
        };
        assert!(cfg.validate().is_err());
    }
}
