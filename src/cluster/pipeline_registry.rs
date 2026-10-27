//! Scheduled pipeline keys come from the caller's YAML `Config`.

use skippr_lease::{DurableError, PipelineKey};

use crate::helpers::configuration::Config;

pub fn scheduled_pipeline_keys(cfg: &Config) -> Result<Vec<PipelineKey>, DurableError> {
    let tenant = cfg.get_tenant();
    let workspace = cfg.get_workspace_name();
    let mut keys = Vec::new();
    for name in cfg.pipelines.keys() {
        keys.push(
            PipelineKey::new(&tenant, &workspace, name)
                .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?,
        );
    }
    Ok(keys)
}

#[cfg(test)]
mod yaml_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scheduled_pipeline_keys_use_caller_config() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-a", "tenant": "ten-a" },
            "pipelines": {
                "orders": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let keys = scheduled_pipeline_keys(&config).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].tenant(), "ten-a");
        assert_eq!(keys[0].workspace(), "ws-a");
        assert_eq!(keys[0].pipeline(), "orders");
    }

    #[test]
    fn yaml_path_does_not_reload_config() {
        let prod = include_str!("pipeline_registry.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            !prod.contains("try_build_config"),
            "yaml pipeline keys must use Session-owned Config"
        );
    }
}
