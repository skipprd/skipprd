use crate::progress_controller::PhaseTransition;
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::SuiteCtx;

use crate::providers::{SkipprOutputConfig, SkipprPipelineConfig};

impl DataEngineerSuite {
    pub(super) async fn execute_el_discover_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        let skippr = crate::ctx_ext::sctx_skippr(sctx).ok_or_else(|| {
            "skippr provider not configured. enable providers.el and restart.".to_string()
        })?;

        let cfg = sctx
            .resolved_config()
            .as_ref()
            .and_then(|c| crate::de_config::de_config_from_resolved(c))
            .ok_or_else(|| "resolved config missing for EL discover".to_string())?;

        let pipeline_name = sctx.scope().project_id.as_str();

        let output_config = warehouse_to_output_config(&cfg.warehouse);

        let pipeline_config = SkipprPipelineConfig {
            pipeline_name: pipeline_name.to_string(),
            skippr_input: cfg.el.skippr_input.clone(),
            output_plugin: output_config,
            schema_sink: None,
        };

        // 1. Write skipprd.yml
        skippr
            .write_pipeline_config(sctx.scope(), &pipeline_config)
            .await
            .map_err(|e| format!("failed to write skippr pipeline config: {}", e))?;

        // 2. Run skippr discover
        let discover_result = skippr
            .discover_pipeline(sctx.scope(), pipeline_name)
            .await
            .map_err(|e| format!("skippr discover failed: {}", e))?;

        if !discover_result.ok {
            let errs = discover_result.errors.join("; ");
            return Err(format!("skippr discover reported errors: {}", errs).into());
        }

        // 3. Use the compact skipprd-owned discover summary. The React adapter
        // intentionally does not read metadata files or per-namespace stdout payloads.
        // Table materialization happens in the subsequent EL sync phase, not during discover.
        let namespaces_count = discover_result.namespaces_count;
        if namespaces_count == 0 {
            return Err("skippr discover completed but found no namespaces"
                .to_string()
                .into());
        }

        let total_fields = discover_result.total_fields;

        tracing::info!(
            namespaces = namespaces_count,
            total_fields = total_fields,
            pipeline = pipeline_name,
            "EL discover complete"
        );

        crate::phase_contract::commit_metered_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::ElDiscover),
            crate::phase_contract::PhaseDecision::forward(
                control_flow::Phase::ElSync,
                Some(PhaseTransition::ElDiscoverOk { namespaces_count }),
            ),
            vec![crate::metering::UsageEvent::FieldsDiscovered {
                count: total_fields,
                project_id: pipeline_name.to_string(),
            }],
            crate::metering::global_metering(),
        )
        .await?;

        Ok(PhaseOutcome::TransitionCommitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::llm::NullModel;
    use react_core::provider_traits::NullSecretsProvider;
    use react_core::resolved_config::{
        LlmResolved, ReactResolvedConfig, ServerResolved, StorageMode, StorageResolved,
    };
    use react_core::scope::RequestScope;
    use react_core::session::ThreadStore;
    use react_core::suite::SuiteCtx;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[derive(Default)]
    struct MockSkipprProvider {
        discover_result: crate::providers::SkipprDiscoverResult,
    }

    #[async_trait]
    impl crate::providers::SkipprProvider for MockSkipprProvider {
        async fn write_pipeline_config(
            &self,
            _scope: &RequestScope,
            _config: &crate::providers::SkipprPipelineConfig,
        ) -> Result<String, String> {
            Ok("skipprd.yaml".to_string())
        }

        async fn discover_pipeline(
            &self,
            _scope: &RequestScope,
            _pipeline: &str,
        ) -> Result<crate::providers::SkipprDiscoverResult, String> {
            Ok(self.discover_result.clone())
        }

        async fn show_pipeline(
            &self,
            _scope: &RequestScope,
            _pipeline: &str,
        ) -> Result<crate::providers::SkipprPipelineStatus, String> {
            Err("show_pipeline should not run during discover".to_string())
        }

        async fn load_schema(
            &self,
            _scope: &RequestScope,
            _pipeline: &str,
            _schema_json_path: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn sync_pipeline(
            &self,
            _scope: &RequestScope,
            _pipeline: &str,
        ) -> Result<crate::providers::SkipprSyncResult, String> {
            Ok(crate::providers::SkipprSyncResult::default())
        }
    }

    fn test_suite_ctx() -> SuiteCtx {
        let scope = RequestScope::parse("tenant", "dev", "test4").expect("valid test scope");
        SuiteCtx::new(
            Arc::new(InMemoryStorageAdapter::default()),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope,
            Arc::new(DefaultKeyspace::new("test-bucket".to_string())),
        )
    }

    fn resolved_config(scope: RequestScope) -> Arc<ReactResolvedConfig> {
        Arc::new(ReactResolvedConfig {
            server: ServerResolved { port: 0 },
            storage: StorageResolved {
                mode: StorageMode::Local,
                bucket: None,
                path: Some("./.skippr".to_string()),
                s3_credentials: None,
            },
            scope,
            llm: LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": {
                    "kind": "snowflake",
                    "container": "analytics",
                    "namespace": "raw",
                    "extras": {}
                },
                "catalog": { "enabled": false },
                "dbt": { "enabled": false },
                "vector": { "enabled": false },
                "el": {
                    "enabled": true,
                    "skippr_binary": "skipprd",
                    "skippr_input": { "kind": "s3" }
                }
            }),
        })
    }

    #[tokio::test]
    async fn discover_phase_transitions_to_sync_without_show_pipeline_tables() {
        let mut sctx = test_suite_ctx();
        sctx.set_resolved_config(Some(resolved_config(sctx.scope().clone())));
        sctx.set_capability(Arc::new(crate::ctx_ext::SkipprCap(Arc::new(
            MockSkipprProvider {
                discover_result: crate::providers::SkipprDiscoverResult {
                    ok: true,
                    namespaces_count: 1,
                    total_fields: 1,
                    errors: vec![],
                },
            },
        ))));

        let store = ThreadStore::new(
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
        );
        let thread_id = "tid-discover";

        let outcome = DataEngineerSuite::execute_el_discover_phase(&store, thread_id, &sctx)
            .await
            .expect("discover should transition to sync");
        assert!(matches!(outcome, PhaseOutcome::TransitionCommitted));

        let state = crate::progress_controller::ExecutionState::load_strict(
            &store.control_store(),
            thread_id,
        )
        .await
        .expect("control store read should succeed")
        .expect("execution state should be written");
        assert_eq!(
            state.phase.current_phase,
            crate::control_flow::Phase::ElSync
        );
    }
}

fn warehouse_to_output_config(wh: &crate::de_config::WarehouseResolved) -> SkipprOutputConfig {
    use crate::de_config::WarehouseKind;
    let kind = match wh.kind {
        WarehouseKind::Snowflake => "snowflake",
        WarehouseKind::Athena => "athena",
        WarehouseKind::Postgres => "postgres",
        WarehouseKind::Bigquery => "bigquery",
        WarehouseKind::Mssql => "mssql",
        WarehouseKind::Databricks => "databricks",
        WarehouseKind::Synapse => "synapse",
        WarehouseKind::Redshift => "redshift",
        WarehouseKind::Clickhouse => "clickhouse",
        WarehouseKind::Motherduck => "motherduck",
    };
    SkipprOutputConfig {
        kind: kind.to_string(),
        account: None,
        database: Some(wh.container.clone()).filter(|s| !s.is_empty()),
        schema: Some(wh.namespace.clone()).filter(|s| !s.is_empty()),
        warehouse: wh
            .extras
            .get("warehouse")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        role: wh
            .extras
            .get("role")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}
