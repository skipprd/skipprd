use std::collections::HashMap;

use serde::Deserialize;
use serde_derive::Serialize;

use crate::target::normalize_target;

pub const MAX_BACKLINK_LIMIT: u32 = 1000;
pub const MAX_OFFSET: u32 = 20_000;
pub const MAX_INTERSECTION_TARGETS: usize = 20;
pub const MAX_EXCLUDE_TARGETS: usize = 10;
pub const DISCOVER_LIMIT: u32 = 5;
pub const DISCOVER_MAX_PAGES: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Both,
    Backlinks,
    PageIntersection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BacklinkJob {
    pub target: String,
    #[serde(default)]
    pub job_tag: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub backlinks_status_type: Option<String>,
    #[serde(default)]
    pub filters: Option<serde_json::Value>,
    #[serde(default)]
    pub order_by: Option<Vec<String>>,
    #[serde(default)]
    pub max_pages: Option<u32>,
    #[serde(default)]
    pub include_subdomains: Option<bool>,
    #[serde(default)]
    pub exclude_internal_backlinks: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntersectionJob {
    pub name: String,
    pub targets: HashMap<String, String>,
    #[serde(default)]
    pub exclude_targets: Option<Vec<String>>,
    #[serde(default)]
    pub intersection_mode: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub order_by: Option<Vec<String>>,
    #[serde(default)]
    pub max_pages: Option<u32>,
    #[serde(default)]
    pub filters: Option<serde_json::Value>,
    #[serde(default)]
    pub internal_list_limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataForSeoBacklinksPluginConfig {
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub run_mode: RunMode,
    #[serde(default)]
    pub backlink_jobs: Vec<BacklinkJob>,
    #[serde(default)]
    pub intersection_jobs: Vec<IntersectionJob>,
    #[serde(default)]
    pub rank_scale: Option<String>,
    #[serde(default = "default_request_interval_ms")]
    pub request_interval_ms: u64,
    #[serde(default = "default_max_api_retries")]
    pub max_api_retries: u32,
}

fn default_request_interval_ms() -> u64 {
    200
}

fn default_max_api_retries() -> u32 {
    8
}

impl DataForSeoBacklinksPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.run_mode != RunMode::PageIntersection && self.backlink_jobs.is_empty() {
            if self.run_mode == RunMode::Backlinks {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "backlink_jobs must include at least one job when run_mode is backlinks",
                ));
            }
        }
        if self.run_mode != RunMode::Backlinks && self.intersection_jobs.is_empty() {
            if self.run_mode == RunMode::PageIntersection {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "intersection_jobs must include at least one job when run_mode is page_intersection",
                ));
            }
        }
        if self.run_mode == RunMode::Both
            && self.backlink_jobs.is_empty()
            && self.intersection_jobs.is_empty()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "configure at least one backlink_jobs or intersection_jobs entry",
            ));
        }

        for job in &self.backlink_jobs {
            normalize_target(&job.target).map_err(std::io::Error::other)?;
            if let Some(limit) = job.limit {
                if limit == 0 || limit > MAX_BACKLINK_LIMIT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("backlink job limit must be 1..={MAX_BACKLINK_LIMIT}"),
                    ));
                }
            }
        }

        for job in &self.intersection_jobs {
            if job.name.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "intersection job name is required",
                ));
            }
            if job.targets.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("intersection job '{}' requires targets", job.name),
                ));
            }
            if job.targets.len() > MAX_INTERSECTION_TARGETS {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "intersection job '{}' has {} targets (max {MAX_INTERSECTION_TARGETS})",
                        job.name,
                        job.targets.len()
                    ),
                ));
            }
            if job
                .exclude_targets
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0)
                > MAX_EXCLUDE_TARGETS
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "intersection job '{}' has too many exclude_targets (max {MAX_EXCLUDE_TARGETS})",
                        job.name
                    ),
                ));
            }
            for target in job.targets.values() {
                normalize_target(target).map_err(std::io::Error::other)?;
            }
            if let Some(excludes) = &job.exclude_targets {
                for target in excludes {
                    normalize_target(target).map_err(std::io::Error::other)?;
                }
            }
            if let Some(limit) = job.limit {
                if limit == 0 || limit > MAX_BACKLINK_LIMIT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("intersection job limit must be 1..={MAX_BACKLINK_LIMIT}"),
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn resolve_credentials(&self) -> Result<(String, String), std::io::Error> {
        let login = self
            .login
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("DATAFORSEO_LOGIN")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "DataForSEO login is required (config.login or DATAFORSEO_LOGIN)",
                )
            })?;
        let password = self
            .password
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("DATAFORSEO_PASSWORD")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "DataForSEO password is required (config.password or DATAFORSEO_PASSWORD)",
                )
            })?;
        Ok((login, password))
    }

    pub fn site_label(&self) -> String {
        self.site
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| {
                self.backlink_jobs
                    .first()
                    .map(|j| j.target.clone())
                    .or_else(|| {
                        self.intersection_jobs
                            .first()
                            .and_then(|j| j.targets.values().next().cloned())
                    })
            })
            .unwrap_or_else(|| "default".to_string())
    }
}

impl BacklinkJob {
    pub fn job_id(&self) -> String {
        self.job_tag
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.target.clone())
    }

    pub fn effective_limit(&self, discover: bool) -> u32 {
        if discover {
            return DISCOVER_LIMIT;
        }
        self.limit.unwrap_or(100).clamp(1, MAX_BACKLINK_LIMIT)
    }

    pub fn effective_max_pages(&self, discover: bool) -> u32 {
        if discover {
            return DISCOVER_MAX_PAGES;
        }
        self.max_pages.unwrap_or(5).max(1)
    }
}

impl IntersectionJob {
    pub fn effective_limit(&self, discover: bool) -> u32 {
        if discover {
            return DISCOVER_LIMIT;
        }
        self.limit.unwrap_or(100).clamp(1, MAX_BACKLINK_LIMIT)
    }

    pub fn effective_max_pages(&self, discover: bool) -> u32 {
        if discover {
            return DISCOVER_MAX_PAGES;
        }
        self.max_pages.unwrap_or(5).max(1)
    }
}
