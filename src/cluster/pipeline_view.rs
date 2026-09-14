use std::path::PathBuf;

use crate::buffer::compaction_transaction::SinkRetrySemantics;
use crate::helpers::configuration::{Config, Pipeline};
use crate::helpers::wal_storage::ConfigError;
use crate::plugins::cdc::SinkCapability;
use skippr_lease::{PipelineKey, PipelinePaths};

/// Immutable per-pipeline view. Replica/query/scheduler code uses this instead of
/// mutating process-global `PIPELINE_NAME`.
#[derive(Clone, Debug)]
pub struct PipelineConfigView {
    pub key: PipelineKey,
    pub data_root: PathBuf,
    pub source_plugin: String,
    pub sink_plugin: String,
    pub schema_plugin: Option<String>,
    pub sink_ref: Option<String>,
    pub iceberg: bool,
    pub flatten_events: bool,
}

impl PipelineConfigView {
    pub fn for_name(config: &Config, pipeline: &str) -> Result<Self, ConfigError> {
        let pipeline_cfg = config
            .pipelines
            .get(pipeline)
            .ok_or_else(|| ConfigError::PipelineNotFound(pipeline.to_string()))?;
        let tenant = tenant_from_config(config);
        let workspace = workspace_from_config(config);
        let key = PipelineKey::new(tenant, workspace, pipeline)
            .map_err(|err| ConfigError::InvalidIdentity(err.to_string()))?;
        let data_root = data_root_for_pipeline(config, pipeline_cfg);
        let (sink_ref, sink_plugin, schema_plugin) = resolve_sink(config, pipeline_cfg);
        let source_plugin = resolve_source(config, pipeline_cfg);
        let iceberg = sink_plugin.eq_ignore_ascii_case("Iceberg");
        let flatten_events = pipeline_cfg
            .transform
            .as_ref()
            .and_then(|transform| transform.flatten_events.as_ref())
            .map(|value| Config::truth_value(value))
            .unwrap_or(false);
        Ok(Self {
            key,
            data_root,
            source_plugin,
            sink_plugin,
            schema_plugin,
            sink_ref,
            iceberg,
            flatten_events,
        })
    }

    pub fn for_registry(config: &Config, key: &PipelineKey) -> Result<Self, ConfigError> {
        if !config.pipelines.contains_key(key.pipeline()) {
            return Err(ConfigError::PipelineNotFound(key.pipeline().to_string()));
        }
        let mut view = Self::for_name(config, key.pipeline())?;
        view.key = key.clone();
        Ok(view)
    }

    pub fn key(&self) -> &PipelineKey {
        &self.key
    }

    pub fn paths(&self) -> Result<PipelinePaths, ConfigError> {
        PipelinePaths::new(&self.data_root, &self.key)
            .map_err(|err| ConfigError::InvalidIdentity(err.to_string()))
    }

    pub fn sink_capability(&self) -> Option<&'static SinkCapability> {
        crate::plugins::cdc::sink_capabilities::by_name(&self.sink_plugin)
    }

    pub fn validate_clustered_sink(&self) -> Result<(), ConfigError> {
        let Some(capability) = self.sink_capability() else {
            return Err(ConfigError::ClusteredSinkNotIdempotent {
                plugin: self.sink_plugin.clone(),
                semantics: "unknown".into(),
            });
        };
        if !capability.retry_semantics.requires_idempotent_replay() {
            return Err(ConfigError::ClusteredSinkNotIdempotent {
                plugin: capability.name.to_string(),
                semantics: format!("{:?}", capability.retry_semantics),
            });
        }
        if capability.grouping_support.is_none() {
            return Err(ConfigError::ClusteredSinkGroupingUnsupported {
                plugin: capability.name.to_string(),
            });
        }
        let _ = SinkRetrySemantics::equals(capability.retry_semantics, capability.retry_semantics);
        Ok(())
    }
}

fn tenant_from_config(config: &Config) -> String {
    config
        .skippr
        .as_ref()
        .and_then(|s| s.tenant.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("TENANT").unwrap_or_else(|_| "default".into()))
}

fn workspace_from_config(config: &Config) -> String {
    config
        .skippr
        .as_ref()
        .and_then(|s| s.workspace.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("WORKSPACE_NAME").unwrap_or_else(|_| "default".into()))
}

fn data_root_for_pipeline(config: &Config, pipeline: &Pipeline) -> PathBuf {
    let default = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into());
    let dir = pipeline
        .data_dir
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| config.skippr.as_ref().and_then(|_| None))
        .unwrap_or(default);
    PathBuf::from(dir)
}

fn resolve_source(config: &Config, pipeline: &Pipeline) -> String {
    let Some(data_source_ref) = pipeline.data_source.as_ref() else {
        return String::new();
    };
    let name = Config::parse_registry_ref(data_source_ref, "data_sources")
        .unwrap_or_else(|_| data_source_ref.clone());
    config
        .data_sources
        .as_ref()
        .and_then(|sources| sources.get(&name))
        .and_then(|entry| entry.plugin_name())
        .unwrap_or_default()
}

fn resolve_sink(config: &Config, pipeline: &Pipeline) -> (Option<String>, String, Option<String>) {
    let Some(data_sink_ref) = pipeline.data_sink.as_ref() else {
        return (None, String::new(), None);
    };
    let name = Config::parse_registry_ref(data_sink_ref, "data_sinks")
        .unwrap_or_else(|_| data_sink_ref.clone());
    match config
        .data_sinks
        .as_ref()
        .and_then(|sinks| sinks.get(&name))
    {
        Some(entry) => (
            Some(data_sink_ref.clone()),
            entry.config.plugin_name().unwrap_or_default(),
            entry.schema_sink.clone(),
        ),
        None => (Some(data_sink_ref.clone()), String::new(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clustered_rejects_at_least_once_sink() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "S3".into(),
            sink_plugin: "Stdout".into(),
            schema_plugin: None,
            sink_ref: None,
            iceberg: false,
            flatten_events: false,
        };
        assert!(view.validate_clustered_sink().is_err());
        assert_eq!(
            crate::plugins::cdc::sink_capabilities::by_name("Stdout")
                .unwrap()
                .name,
            "Stdout"
        );
    }

    #[test]
    fn clustered_accepts_iceberg() {
        let view = PipelineConfigView {
            key: PipelineKey::new("t", "w", "p").unwrap(),
            data_root: PathBuf::from("/tmp"),
            source_plugin: "S3".into(),
            sink_plugin: "Iceberg".into(),
            schema_plugin: None,
            sink_ref: None,
            iceberg: true,
            flatten_events: false,
        };
        assert!(view.validate_clustered_sink().is_ok());
    }

    #[test]
    fn registry_without_yaml_overlay_fails_closed() {
        let config = Config::new();
        let key = PipelineKey::new("system", "platform", "otel-logs").unwrap();
        let err = PipelineConfigView::for_registry(&config, &key).unwrap_err();
        assert!(matches!(err, ConfigError::PipelineNotFound(_)));
    }
}
