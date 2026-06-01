use serde::Deserialize;
use serde_derive::Serialize;

use crate::origin::host_label;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackedPrompt {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceAiCitationsPluginConfig {
    pub site: String,
    #[serde(default)]
    pub brand_names: Vec<String>,
    pub prompt_list: Vec<TrackedPrompt>,
    pub models: Vec<String>,
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    #[serde(default = "default_max_prompts_per_run")]
    pub max_prompts_per_run: u32,
    #[serde(default = "default_skip_unchanged_responses")]
    pub skip_unchanged_responses: bool,
    #[serde(default)]
    pub openai_base_url: Option<String>,
}

fn default_requests_per_minute() -> u32 {
    10
}

fn default_max_prompts_per_run() -> u32 {
    100
}

fn default_skip_unchanged_responses() -> bool {
    true
}

impl DataSourceAiCitationsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.prompt_list.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "prompt_list must include at least one prompt",
            ));
        }
        for prompt in &self.prompt_list {
            if prompt.id.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "each prompt must have a non-empty id",
                ));
            }
            if prompt.text.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("prompt '{}' must have non-empty text", prompt.id),
                ));
            }
        }
        if self.models.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "models must include at least one model name",
            ));
        }
        for model in &self.models {
            if model.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "model names must be non-empty",
                ));
            }
        }
        if self.max_prompts_per_run == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_prompts_per_run must be >= 1",
            ));
        }
        if self.requests_per_minute == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "requests_per_minute must be >= 1",
            ));
        }
        Ok(())
    }

    pub fn resolved_brand_names(&self, site_origin: &str) -> Vec<String> {
        if !self.brand_names.is_empty() {
            return self.brand_names.clone();
        }
        vec![host_label(site_origin)]
    }

    pub fn api_active(&self) -> bool {
        std::env::var("OPENAI_API_KEY")
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false)
            || std::env::var(crate::client::FIXTURE_ENV)
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
            || std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::FIXTURE_ENV;
    use crate::origin::normalize_site_origin;

    fn valid_cfg() -> DataSourceAiCitationsPluginConfig {
        DataSourceAiCitationsPluginConfig {
            site: "https://example.com".into(),
            brand_names: vec![],
            prompt_list: vec![TrackedPrompt {
                id: "p1".into(),
                text: "q".into(),
                category: None,
                intent: None,
            }],
            models: vec!["gpt-4.1-mini".into()],
            requests_per_minute: 10,
            max_prompts_per_run: 50,
            skip_unchanged_responses: true,
            openai_base_url: None,
        }
    }

    #[test]
    fn prompt_list_required() {
        let mut cfg = valid_cfg();
        cfg.prompt_list.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn site_required() {
        let mut cfg = valid_cfg();
        cfg.site = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_prompt_id_rejected() {
        let mut cfg = valid_cfg();
        cfg.prompt_list[0].id = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_prompt_text_rejected() {
        let mut cfg = valid_cfg();
        cfg.prompt_list[0].text = "".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn models_required() {
        let mut cfg = valid_cfg();
        cfg.models.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_max_prompts_rejected() {
        let mut cfg = valid_cfg();
        cfg.max_prompts_per_run = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_requests_per_minute_rejected() {
        let mut cfg = valid_cfg();
        cfg.requests_per_minute = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn brand_names_fallback_to_site_host() {
        let cfg = valid_cfg();
        let origin = normalize_site_origin(&cfg.site).unwrap();
        assert_eq!(cfg.resolved_brand_names(&origin), vec!["example.com"]);
    }

    #[test]
    fn api_active_with_fixture_env() {
        let _lock = std::sync::Mutex::new(());
        let _guard = _lock.lock().unwrap();
        std::env::set_var(FIXTURE_ENV, "/tmp");
        let cfg = valid_cfg();
        assert!(cfg.api_active());
        std::env::remove_var(FIXTURE_ENV);
    }
}
