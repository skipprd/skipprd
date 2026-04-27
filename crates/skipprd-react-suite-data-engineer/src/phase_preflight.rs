use crate::progress_controller::PhaseTransition;
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::storage::retry_head_etag;
use react_core::suite::SuiteCtx;

impl DataEngineerSuite {
    pub(super) async fn execute_preflight_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        if crate::ctx_ext::sctx_query(sctx).is_none() {
            return Err("data engineer agent requires a warehouse provider configured. configure providers.warehouse and restart.".to_string().into());
        }
        let Some(dbt) = crate::ctx_ext::sctx_dbt(sctx) else {
            return Err("data engineer agent requires a dbt provider configured. enable providers.dbt and restart.".to_string().into());
        };
        {
            let dbt = dbt;
            if let Err(e) = crate::transient_retry::retry_transient_default(
                "preflight_ensure_minimal_project",
                || async { dbt.ensure_minimal_project(sctx.scope()).await },
            )
            .await
            {
                let key = sctx
                    .keyspace()
                    .scoped_key(sctx.scope(), &["dbt", "dbt_project.yml"]);
                return Err(format!(
                    "failed to create the dbt project in storage. expected file: {key}. error: {e}. this is usually an s3 permission/prefix issue."
                ).into());
            }
        }
        let key = sctx
            .keyspace()
            .scoped_key(sctx.scope(), &["dbt", "dbt_project.yml"]);
        match retry_head_etag(sctx.storage().as_ref(), &key).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(format!(
                    "dbt project is incomplete: dbt_project.yml is missing in storage. expected file: {key}. without this file the suite cannot validate/build."
                ).into());
            }
            Err(e) => {
                return Err(format!(
                    "unable to verify presence of dbt_project.yml in storage. expected file: {key}. error: {e}"
                ).into());
            }
        }
        crate::phase_contract::commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::Preflight),
            crate::phase_contract::PhaseDecision::forward(
                control_flow::Phase::CleansePlan,
                Some(PhaseTransition::PreflightOk {
                    dbt_project_key: key.clone(),
                    has_query_provider: crate::ctx_ext::sctx_query(sctx).is_some(),
                    has_dbt_provider: crate::ctx_ext::sctx_dbt(sctx).is_some(),
                }),
            ),
        )
        .await?;
        Ok(PhaseOutcome::TransitionCommitted)
    }
}
