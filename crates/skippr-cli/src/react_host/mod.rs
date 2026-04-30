mod data_engineer;
pub(crate) mod vector;

use std::sync::Arc;

use async_trait::async_trait;
use react_core::resolved_config::{ReactResolvedConfig, StorageMode};
use react_core::suite::{DebugProviderRegistry, SuiteCtx, SuiteRegistry};
use vector::{lance_storage_options_from_credentials, LanceStorageOptions};

#[derive(Clone, Copy, Debug, Default)]
pub struct SkipprHost;

#[async_trait]
impl react::host::HostComposition for SkipprHost {
    fn register_suites(&self, registry: &mut SuiteRegistry) {
        registry.register(react_suite_data_engineer::DataEngineerSuite);
    }

    fn resolve_suite_config(
        &self,
        raw_suite_config: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        react_suite_data_engineer::de_config::resolve_providers_from_yaml(raw_suite_config)
    }

    async fn configure_suite_ctx(
        &self,
        cfg: &ReactResolvedConfig,
        suite_ctx: &mut SuiteCtx,
    ) -> Result<(), String> {
        let keyspace = suite_ctx.keyspace().clone();
        let (lance_uri_prefix, lance_storage_opts) = lance_storage(cfg)?;
        data_engineer::wire_providers(suite_ctx, &keyspace, &lance_uri_prefix, lance_storage_opts)
            .await?;

        let mut debug_reg = DebugProviderRegistry::new();
        debug_reg.register(react_suite_data_engineer::debug::DataEngineerDebugProvider);
        suite_ctx.set_capability(Arc::new(debug_reg));
        Ok(())
    }
}

pub fn resolve_config(
    file: react::config::ReactConfigFile,
    overrides: react::config::ServeOverrides,
) -> Result<ReactResolvedConfig, String> {
    react::config::resolve_config_with(file, overrides, &SkipprHost)
}

pub async fn run_headless(
    cfg: ReactResolvedConfig,
    opts: react::run_engine::HeadlessRunOpts,
) -> i32 {
    react::run_engine::run_headless_with_host(cfg, &SkipprHost, opts).await
}

fn lance_storage(cfg: &ReactResolvedConfig) -> Result<(String, LanceStorageOptions), String> {
    match cfg.storage.mode {
        StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .clone()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            Ok((format!("file://{}", root), LanceStorageOptions::default()))
        }
        StorageMode::S3 => {
            let bucket = cfg
                .storage
                .bucket
                .clone()
                .ok_or_else(|| "missing storage.bucket for s3 mode".to_string())?;
            let options = cfg
                .storage
                .s3_credentials
                .as_ref()
                .map(|creds| {
                    creds
                        .provider
                        .clone()
                        .map(LanceStorageOptions::Refreshable)
                        .unwrap_or_else(|| {
                            LanceStorageOptions::Static(lance_storage_options_from_credentials(
                                creds,
                            ))
                        })
                })
                .unwrap_or_default();
            Ok((format!("s3://{}", bucket), options))
        }
    }
}
