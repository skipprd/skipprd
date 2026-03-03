use crate::data_engineer::{control_flow, DataEngineerSuite, PhaseExecutorOutcome};
use crate::suite::SuiteCtx;
use react_core::control_flow::PhaseReasonCode;
use react_core::session::ThreadStore;

impl DataEngineerSuite {
    pub(super) async fn execute_preflight_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseExecutorOutcome, String> {
        if sctx.query.is_none() {
            return Err("data engineer agent requires a warehouse provider configured. configure providers.warehouse and restart.".to_string());
        }
        if sctx.dbt.is_none() {
            return Err("data engineer agent requires a dbt provider configured. enable providers.dbt and restart.".to_string());
        }
        if let Some(dbt) = sctx.dbt.as_ref() {
            if let Err(e) = dbt.ensure_minimal_project(&sctx.scope).await {
                let key = sctx.keyspace.dbt_project_key(&sctx.scope);
                return Err(format!(
                    "failed to create the dbt project in storage. expected file: {key}. error: {e}. this is usually an s3 permission/prefix issue."
                ));
            }
        }
        let key = sctx.keyspace.dbt_project_key(&sctx.scope);
        match sctx.storage.head_etag(&key).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(format!(
                    "dbt project is incomplete: dbt_project.yml is missing in storage. expected file: {key}. without this file the suite cannot validate/build."
                ));
            }
            Err(e) => {
                return Err(format!(
                    "unable to verify presence of dbt_project.yml in storage. expected file: {key}. error: {e}"
                ));
            }
        }
        crate::data_engineer::phase_contract::commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::Preflight),
            crate::data_engineer::phase_contract::PhaseDecision::forward(
                control_flow::Phase::CleansePlan,
                Some(PhaseReasonCode::PreflightOk),
                Some(serde_json::json!({
                    "dbt_project_key": key,
                    "has_query_provider": sctx.query.is_some(),
                    "has_dbt_provider": sctx.dbt.is_some(),
                })),
            ),
        )
        .await?;
        Ok(PhaseExecutorOutcome::Continue)
    }
}

