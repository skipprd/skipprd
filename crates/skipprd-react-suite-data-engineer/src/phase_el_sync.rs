use crate::progress_controller::PhaseTransition;
use crate::providers::{SkipprOutputConfig, SkipprPipelineConfig};
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::SuiteCtx;

impl DataEngineerSuite {
    pub(super) async fn execute_el_sync_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        let skippr = crate::ctx_ext::sctx_skippr(sctx)
            .ok_or_else(|| "skippr provider not configured for EL sync".to_string())?;

        let cfg = sctx
            .resolved_config()
            .as_ref()
            .and_then(|c| crate::de_config::de_config_from_resolved(c))
            .ok_or_else(|| "resolved config missing for EL sync".to_string())?;

        let pipeline = crate::ctx_ext::sctx_pipeline(sctx)?;
        let pipeline_name = pipeline.as_str();

        let output_config = warehouse_to_output_config(&cfg.warehouse);
        let pipeline_config = SkipprPipelineConfig {
            pipeline_name: pipeline_name.to_string(),
            skippr_input: cfg.el.skippr_input.clone(),
            output_plugin: output_config,
            schema_sink: None,
        };

        skippr
            .write_pipeline_config(sctx.scope(), &pipeline_config)
            .await
            .map_err(|e| format!("failed to write skippr pipeline config: {}", e))?;

        let sync_result = skippr
            .sync_pipeline(sctx.scope(), pipeline_name)
            .await
            .map_err(|e| format!("skippr sync failed: {}", e))?;

        if !sync_result.ok {
            let errs = sync_result.errors.join("; ");
            tracing::error!(errors = %errs, "skippr sync reported errors");
            return Err(format!("skippr sync failed: {}", errs).into());
        }

        tracing::info!(
            tables_synced = sync_result.tables_synced,
            "EL sync complete"
        );

        crate::phase_contract::commit_metered_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::ElSync),
            crate::phase_contract::PhaseDecision::forward(
                control_flow::Phase::ElVerify,
                Some(PhaseTransition::ElSyncOk {
                    tables_synced: sync_result.tables_synced,
                }),
            ),
            vec![crate::metering::UsageEvent::TablesSynced {
                count: sync_result.tables_synced as u64,
                project_id: pipeline_name.to_string(),
            }],
            crate::metering::global_metering(),
        )
        .await?;

        Ok(PhaseOutcome::TransitionCommitted)
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
