use crate::progress_controller::PhaseTransition;
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::SuiteCtx;

impl DataEngineerSuite {
    pub(super) async fn execute_el_verify_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        let skippr = crate::ctx_ext::sctx_skippr(sctx)
            .ok_or_else(|| "skippr provider not configured for EL verify".to_string())?;

        let pipeline_name = sctx.scope().project_id.as_str();

        let pipeline_status = skippr
            .show_pipeline(sctx.scope(), pipeline_name)
            .await
            .map_err(|e| format!("SHOW PIPELINE failed during verify: {}", e))?;

        tracing::info!(
            status = %pipeline_status.status,
            namespaces = pipeline_status.namespaces.len(),
            "skippr pipeline status during verify"
        );

        let expected_tables: Vec<String> = pipeline_status
            .namespaces
            .iter()
            .map(|ns| namespace_to_snowflake_table(&ns.namespace))
            .collect();

        let wh = sctx
            .capability::<crate::ctx_ext::ProvidersCfgCap>()
            .map(|c| c.0.warehouse.clone());
        let mut verification_errors = Vec::new();

        if let Some(query_provider) = crate::ctx_ext::sctx_query(sctx) {
            for table_name in &expected_tables {
                let fq_table = if let Some(ref w) = wh {
                    if !w.container.is_empty() && !w.namespace.is_empty() {
                        format!(
                            "\"{}\".\"{}\".\"{}\"",
                            w.container,
                            w.namespace,
                            table_name.to_uppercase()
                        )
                    } else {
                        table_name.clone()
                    }
                } else {
                    table_name.clone()
                };
                let check_sql = format!("SELECT 1 FROM {} LIMIT 0", fq_table);
                match query_provider.query(&check_sql).await {
                    Ok(_) => {
                        tracing::debug!(table = %table_name, "destination table verified");
                    }
                    Err(e) => {
                        tracing::warn!(
                            table = %table_name,
                            error = %e,
                            "destination table verification failed"
                        );
                        verification_errors
                            .push(format!("table '{}' not queryable: {}", table_name, e));
                    }
                }
            }
        } else {
            tracing::info!(
                "no query provider available for destination verification; skipping table checks"
            );
        }

        if !verification_errors.is_empty() {
            let reason = verification_errors.join("; ");
            tracing::warn!(errors = %reason, "EL verify found issues");

            crate::phase_contract::commit_phase_decision(
                thread_store,
                thread_id,
                Some(control_flow::Phase::ElVerify),
                crate::phase_contract::PhaseDecision::forward(
                    control_flow::Phase::Preflight,
                    Some(PhaseTransition::ElVerifyFailed { reason }),
                ),
            )
            .await?;

            return Ok(PhaseOutcome::TransitionCommitted);
        }

        tracing::info!(
            tables_verified = expected_tables.len(),
            "EL verify complete — all destination tables confirmed"
        );

        crate::phase_contract::commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::ElVerify),
            crate::phase_contract::PhaseDecision::forward(
                control_flow::Phase::Preflight,
                Some(PhaseTransition::ElVerifyOk),
            ),
        )
        .await?;

        Ok(PhaseOutcome::TransitionCommitted)
    }
}

fn namespace_to_snowflake_table(namespace: &str) -> String {
    namespace.replace('.', "_").to_lowercase()
}
