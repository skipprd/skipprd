use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix};
use std::time::Duration;

use crate::plan_grounding::{
    ensure_expected_model_paths_cleanse, ensure_expected_model_paths_model,
    prune_cleanse_plan_to_grounded_raw_datasets,
};
use crate::plan_types::*;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan storage failed: {0}")]
    StorageFailed(String),
    #[error("plan deserialization failed for '{key}': {detail}")]
    DeserializeFailed { key: String, detail: String },
    #[error("plan save failed: {0}")]
    SaveFailed(String),
    #[error("plan key missing on plan document")]
    MissingPlanKey,
    #[error("plan grounding failed: {0}")]
    GroundingFailed(String),
}

fn thread_dir(ctx: &AgentCtx) -> String {
    ctx.thread_id()
        .as_deref()
        .unwrap_or("no_thread")
        .trim()
        .to_string()
}

fn utc_timestamp_compact() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn plans_thread_prefix(ctx: &AgentCtx) -> String {
    let root = ctx
        .keyspace()
        .threads_prefix(ctx.scope())
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let tid = thread_dir(ctx);
    format!("{}/plans/{}/", root, tid)
}

const PLAN_SAVE_RETRY_DELAYS_MS: [u64; 3] = [0, 25, 75];

async fn save_plan_bytes_with_retry(
    ctx: &AgentCtx,
    plan_key: &str,
    bytes: &[u8],
    plan_kind: &str,
) -> Result<(), PlanError> {
    let mut last_err: Option<String> = None;
    for (attempt_idx, delay_ms) in PLAN_SAVE_RETRY_DELAYS_MS.iter().enumerate() {
        if attempt_idx > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }
        match ctx
            .storage()
            .put_bytes(plan_key, bytes, "application/json")
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                let err = e.to_string();
                if attempt_idx + 1 < PLAN_SAVE_RETRY_DELAYS_MS.len() {
                    tracing::warn!(
                        plan_kind,
                        plan_key,
                        attempt = attempt_idx + 1,
                        max_attempts = PLAN_SAVE_RETRY_DELAYS_MS.len(),
                        error = %err,
                        "plan save failed; retrying"
                    );
                }
                last_err = Some(err);
            }
        }
    }

    Err(PlanError::SaveFailed(format!(
        "failed to persist {plan_kind} plan '{plan_key}' after {} attempts: {}",
        PLAN_SAVE_RETRY_DELAYS_MS.len(),
        last_err.unwrap_or_else(|| "unknown storage failure".to_string())
    )))
}

pub fn new_cleanse_plan_key(ctx: &AgentCtx) -> String {
    let pref = plans_thread_prefix(ctx);
    format!("{}{}_cleanse.json", pref, utc_timestamp_compact())
}

pub fn new_model_plan_key(ctx: &AgentCtx) -> String {
    let pref = plans_thread_prefix(ctx);
    format!("{}{}_model.json", pref, utc_timestamp_compact())
}

async fn list_plan_keys(ctx: &AgentCtx, suffix: &str) -> Result<Vec<String>, PlanError> {
    let pref = plans_thread_prefix(ctx);
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .map_err(|e| PlanError::StorageFailed(e.to_string()))?;
    keys.retain(|k| k.ends_with(suffix));
    keys.sort();
    Ok(keys)
}

/// Load the cleanse plan for this thread.
///
/// Prefers the **newest** deserializeable non-terminal plan (keys are UTC-sorted; we scan
/// newest-first so a superseding draft wins over older broken or abandoned JSON).
/// If none exists (e.g. every snapshot is terminal), falls back to the newest plan key so
/// progress and context are always available.
pub async fn load_cleanse_plan(ctx: &AgentCtx) -> Result<Option<CleansePlan>, PlanError> {
    let keys = list_plan_keys(ctx, "_cleanse.json").await?;
    for k in keys.iter().rev() {
        let bytes = match retry_get_bytes(ctx.storage().as_ref(), k).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("skipping plan key {k}: storage read failed: {e}");
                continue;
            }
        };
        match serde_json::from_slice::<CleansePlan>(&bytes) {
            Ok(p) if !p.status.is_terminal() => return load_cleanse_plan_by_key(ctx, k).await,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("skipping plan key {k}: deserialization failed: {e}");
            }
        }
    }
    let Some(key) = keys.last() else {
        return Ok(None);
    };
    load_cleanse_plan_by_key(ctx, key).await
}

pub async fn load_cleanse_plan_by_key(
    ctx: &AgentCtx,
    key: &str,
) -> Result<Option<CleansePlan>, PlanError> {
    let bytes = match retry_get_bytes(ctx.storage().as_ref(), key).await {
        Ok(b) => b,
        Err(e) => {
            return Err(PlanError::StorageFailed(format!(
                "failed to read plan key '{key}': {e}"
            )));
        }
    };
    let mut p = serde_json::from_slice::<CleansePlan>(&bytes).map_err(|e| {
        PlanError::DeserializeFailed {
            key: key.to_string(),
            detail: e.to_string(),
        }
    })?;
    if p.plan_key.trim().is_empty() {
        p.plan_key = key.to_string();
    }
    let changed = ensure_expected_model_paths_cleanse(Some(ctx), &mut p);
    if changed {
        if let Err(e) = save_cleanse_plan(ctx, &p).await {
            tracing::warn!("failed to persist canonicalized cleanse plan paths: {e}");
        }
    }
    Ok(Some(p))
}

/// Persist a cleanse plan without re-validating grounding. Used for mid-execution
/// mutations on plans that were already grounded at creation time.
pub async fn save_cleanse_plan(ctx: &AgentCtx, plan: &CleansePlan) -> Result<(), PlanError> {
    if plan.plan_key.trim().is_empty() {
        return Err(PlanError::MissingPlanKey);
    }
    let bytes =
        serde_json::to_vec_pretty(plan).map_err(|e| PlanError::SaveFailed(e.to_string()))?;
    save_plan_bytes_with_retry(ctx, &plan.plan_key, &bytes, "cleanse").await
}

/// Validate grounding and persist. Returns the `GroundedCleansePlan` proof so
/// callers can carry forward the evidence of successful grounding.
pub async fn save_cleanse_plan_grounded(
    ctx: &AgentCtx,
    plan: &CleansePlan,
    allowed_raw: &std::collections::BTreeSet<String>,
) -> Result<GroundedCleansePlan, PlanError> {
    let mut candidate = plan.clone();
    ensure_expected_model_paths_cleanse(Some(ctx), &mut candidate);
    prune_cleanse_plan_to_grounded_raw_datasets(&mut candidate, allowed_raw);
    let grounded = GroundedCleansePlan::try_from(candidate).map_err(PlanError::GroundingFailed)?;
    save_cleanse_plan(ctx, &grounded.0).await?;
    Ok(grounded)
}

/// Load the model plan for this thread.
///
/// Prefers the **newest** deserializeable non-terminal plan. If none exists, falls back to the
/// newest plan key.
pub async fn load_model_plan(ctx: &AgentCtx) -> Result<Option<ModelPlan>, PlanError> {
    let keys = list_plan_keys(ctx, "_model.json").await?;
    for k in keys.iter().rev() {
        let bytes = match retry_get_bytes(ctx.storage().as_ref(), k).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("skipping plan key {k}: storage read failed: {e}");
                continue;
            }
        };
        match serde_json::from_slice::<ModelPlan>(&bytes) {
            Ok(p) if !p.status.is_terminal() => return load_model_plan_by_key(ctx, k).await,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("skipping plan key {k}: deserialization failed: {e}");
            }
        }
    }
    let Some(key) = keys.last() else {
        return Ok(None);
    };
    load_model_plan_by_key(ctx, key).await
}

pub async fn load_model_plan_by_key(
    ctx: &AgentCtx,
    key: &str,
) -> Result<Option<ModelPlan>, PlanError> {
    let bytes = match retry_get_bytes(ctx.storage().as_ref(), key).await {
        Ok(b) => b,
        Err(e) => {
            return Err(PlanError::StorageFailed(format!(
                "failed to read plan key '{key}': {e}"
            )));
        }
    };
    let mut p =
        serde_json::from_slice::<ModelPlan>(&bytes).map_err(|e| PlanError::DeserializeFailed {
            key: key.to_string(),
            detail: e.to_string(),
        })?;
    if p.plan_key.trim().is_empty() {
        p.plan_key = key.to_string();
    }
    ensure_expected_model_paths_model(&mut p);
    Ok(Some(p))
}

/// Persist a model plan without re-validating grounding. Used for mid-execution
/// mutations (checklist updates, batch progress, status changes) on plans that
/// were already grounded at creation time via `save_model_plan_grounded`.
pub async fn save_model_plan(ctx: &AgentCtx, plan: &ModelPlan) -> Result<(), PlanError> {
    if plan.plan_key.trim().is_empty() {
        return Err(PlanError::MissingPlanKey);
    }
    let bytes =
        serde_json::to_vec_pretty(plan).map_err(|e| PlanError::SaveFailed(e.to_string()))?;
    save_plan_bytes_with_retry(ctx, &plan.plan_key, &bytes, "model").await
}

/// Validate grounding against the staging model allowlist and persist.
/// Returns the `GroundedModelPlan` proof so callers can carry forward the
/// evidence of successful grounding.
pub async fn save_model_plan_grounded(
    ctx: &AgentCtx,
    plan: &ModelPlan,
    allowed_staging_models: &std::collections::BTreeSet<String>,
) -> Result<GroundedModelPlan, PlanError> {
    let candidate = plan.clone();
    let grounded = GroundedModelPlan::ground(candidate, allowed_staging_models)
        .map_err(PlanError::GroundingFailed)?;
    save_model_plan(ctx, &grounded.0).await?;
    Ok(grounded)
}

/// Read the active plan's `PlanSnapshot::stripped_artifacts` for prompt assembly.
///
/// Prefers the model plan (later in the pipeline) and falls back to the cleanse plan, mirroring
/// the write-side ordering in [`persist_stripped_artifacts`]. Returns an empty vector when no
/// active plan exists, when neither plan has stripped artifacts, or on any storage/parse error
/// (the strip notice is informational and must never block prompt assembly).
pub async fn load_active_stripped_artifacts(ctx: &AgentCtx) -> Vec<StrippedArtifact> {
    if let Ok(Some(plan)) = load_model_plan(ctx).await {
        if !plan.project_snapshot.stripped_artifacts.is_empty() {
            return plan.project_snapshot.stripped_artifacts;
        }
    }
    if let Ok(Some(plan)) = load_cleanse_plan(ctx).await {
        if !plan.project_snapshot.stripped_artifacts.is_empty() {
            return plan.project_snapshot.stripped_artifacts;
        }
    }
    Vec::new()
}

/// Persist a batch of stripped-artifact entries onto the active plan's `PlanSnapshot`.
///
/// The artifacts come from the dbt provider's sanitizer (see
/// `crate::providers::DbtProvider::ensure_minimal_project`) and need to flow into whatever plan
/// the agent is currently executing so the next author/repair turn surfaces them in the
/// stripped-content prompt section. Best-effort: failures are logged and swallowed (a strip
/// notification is informational, never the cause of a phase failure).
pub async fn persist_stripped_artifacts(ctx: &AgentCtx, artifacts: Vec<StrippedArtifact>) {
    if artifacts.is_empty() {
        return;
    }
    // Prefer the model plan if one is active (later in the pipeline). Fall back to the cleanse
    // plan otherwise. We update at most one plan to avoid duplicate notices.
    match load_model_plan(ctx).await {
        Ok(Some(mut plan)) => {
            plan.project_snapshot
                .extend_stripped_artifacts(artifacts.clone());
            if let Err(e) = save_model_plan(ctx, &plan).await {
                tracing::warn!("failed to persist stripped artifacts onto model plan: {e}");
            }
            return;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("failed to load model plan for stripped-artifact persistence: {e}")
        }
    }
    match load_cleanse_plan(ctx).await {
        Ok(Some(mut plan)) => {
            plan.project_snapshot.extend_stripped_artifacts(artifacts);
            if let Err(e) = save_cleanse_plan(ctx, &plan).await {
                tracing::warn!("failed to persist stripped artifacts onto cleanse plan: {e}");
            }
        }
        Ok(None) => {
            tracing::debug!("stripped artifacts produced but no active plan exists to record them");
        }
        Err(e) => {
            tracing::warn!("failed to load cleanse plan for stripped-artifact persistence: {e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::agent::{AgentCtx, AgentCtxBuilder, DefaultPolicy};
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{LargeLanguageModel, NullModel};
    use react_core::scope::RequestScope;
    use react_core::storage::{
        ConditionalWriteStatus, StorageAdapter, SAFE_STORAGE_RETRY_DELAYS_MS,
    };
    use react_core::CoreError;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FlakyPutBytesStorage {
        inner: Arc<InMemoryStorageAdapter>,
        target_key: String,
        remaining_put_failures: Arc<Mutex<usize>>,
        put_attempts: Arc<Mutex<usize>>,
        remaining_get_failures: Arc<Mutex<usize>>,
        get_attempts: Arc<Mutex<usize>>,
    }

    impl FlakyPutBytesStorage {
        fn new(target_key: String, remaining_failures: usize) -> Self {
            Self {
                inner: Arc::new(InMemoryStorageAdapter::default()),
                target_key,
                remaining_put_failures: Arc::new(Mutex::new(remaining_failures)),
                put_attempts: Arc::new(Mutex::new(0)),
                remaining_get_failures: Arc::new(Mutex::new(0)),
                get_attempts: Arc::new(Mutex::new(0)),
            }
        }

        fn with_get_failures(target_key: String, remaining_failures: usize) -> Self {
            Self {
                inner: Arc::new(InMemoryStorageAdapter::default()),
                target_key,
                remaining_put_failures: Arc::new(Mutex::new(0)),
                put_attempts: Arc::new(Mutex::new(0)),
                remaining_get_failures: Arc::new(Mutex::new(remaining_failures)),
                get_attempts: Arc::new(Mutex::new(0)),
            }
        }

        fn put_attempts(&self) -> usize {
            *self.put_attempts.lock().expect("attempt mutex poisoned")
        }

        fn get_attempts(&self) -> usize {
            *self.get_attempts.lock().expect("attempt mutex poisoned")
        }
    }

    #[async_trait]
    impl StorageAdapter for FlakyPutBytesStorage {
        async fn get_json(&self, key: &str) -> Result<Value, CoreError> {
            self.inner.get_json(key).await
        }

        async fn put_json(&self, key: &str, value: &Value) -> Result<(), CoreError> {
            self.inner.put_json(key, value).await
        }

        async fn put_json_if_etag_matches(
            &self,
            key: &str,
            value: &Value,
            expected_etag: Option<&str>,
        ) -> Result<ConditionalWriteStatus, CoreError> {
            self.inner
                .put_json_if_etag_matches(key, value, expected_etag)
                .await
        }

        async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, CoreError> {
            if key == self.target_key {
                *self.get_attempts.lock().expect("attempt mutex poisoned") += 1;
                let mut remaining = self
                    .remaining_get_failures
                    .lock()
                    .expect("remaining_get_failures mutex poisoned");
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(CoreError::Storage(format!(
                        "synthetic get_bytes failure for '{key}'"
                    )));
                }
            }
            self.inner.get_bytes(key).await
        }

        async fn put_bytes(
            &self,
            key: &str,
            bytes: &[u8],
            content_type: &str,
        ) -> Result<(), CoreError> {
            if key == self.target_key {
                *self.put_attempts.lock().expect("attempt mutex poisoned") += 1;
                let mut remaining = self
                    .remaining_put_failures
                    .lock()
                    .expect("remaining_put_failures mutex poisoned");
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(CoreError::Storage(format!(
                        "synthetic put_bytes failure for '{key}'"
                    )));
                }
            }
            self.inner.put_bytes(key, bytes, content_type).await
        }

        async fn delete_object(&self, key: &str) -> Result<(), CoreError> {
            self.inner.delete_object(key).await
        }

        async fn head_etag(&self, key: &str) -> Result<Option<String>, CoreError> {
            self.inner.head_etag(key).await
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, CoreError> {
            self.inner.list_prefix(prefix).await
        }
    }

    fn test_ctx(storage: Arc<dyn StorageAdapter>, thread_id: &str) -> AgentCtx {
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        AgentCtxBuilder::new(
            llm,
            storage,
            RequestScope::parse("t", "w", "p").expect("valid test scope"),
            keyspace,
            Arc::new(DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id(thread_id.to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(crate::project_fs::test_helpers::minimal_cfg()))
        .build()
    }

    fn sample_model_plan(plan_key: String) -> crate::plan::ModelPlan {
        let batches = vec![vec!["dim_customers".to_string()]];
        crate::plan::ModelPlan {
            plan_key,
            status: crate::plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: crate::plan::ModelFolder::Marts,
                goal: "build customer dimension".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(crate::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: crate::plan::FieldKind::Clean,
                        lineage: vec![crate::plan::FieldLineage::column(
                            crate::plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            crate::plan::lineage_role::NORMALIZED,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: vec![crate::providers::SemanticClaimRef {
                        claim_id: "candidate_key:test_raw.raw_customers:customer_id"
                            .to_string()
                            .into(),
                        kind: crate::providers::SemanticClaimKind::CandidateKey,
                        status: crate::providers::EvidenceStatus::Observed,
                    }],
                }),
                source_schema: vec![crate::plan::SourceColumnDef {
                    name: "customer_id".to_string(),
                    data_type: "bigint".to_string(),
                }],
                grounded_inputs: vec![crate::plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "catalog.db.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![crate::plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "bigint".to_string(),
                    }],
                }],
                status: crate::plan::TaskStatus::InProgress,
                checklist: crate::plan::canonical_task_checklist(
                    crate::track_spec::TrackKind::Model,
                ),
            }],
            batches: batches.clone(),
            work_groups: crate::plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: crate::plan::PlanProgress::default(),
        }
    }

    #[tokio::test]
    async fn save_model_plan_retries_transient_put_bytes_failures() {
        let plan_key = "t/w/p/plans/tid/retry_model.json".to_string();
        let storage = Arc::new(FlakyPutBytesStorage::new(
            plan_key.clone(),
            PLAN_SAVE_RETRY_DELAYS_MS.len() - 1,
        ));
        let ctx = test_ctx(storage.clone(), "tid");
        let plan = sample_model_plan(plan_key.clone());

        save_model_plan(&ctx, &plan)
            .await
            .expect("transient save failures should be retried");

        assert_eq!(storage.put_attempts(), PLAN_SAVE_RETRY_DELAYS_MS.len());
        let bytes = ctx
            .storage()
            .get_bytes(&plan_key)
            .await
            .expect("plan should be persisted after retries");
        let saved = serde_json::from_slice::<crate::plan::ModelPlan>(&bytes)
            .expect("persisted bytes should deserialize");
        assert_eq!(saved.plan_key, plan_key);
    }

    #[tokio::test]
    async fn load_model_plan_by_key_retries_transient_get_bytes_failures() {
        let plan_key = "t/w/p/plans/tid/retry_model_read.json".to_string();
        let storage = Arc::new(FlakyPutBytesStorage::with_get_failures(
            plan_key.clone(),
            SAFE_STORAGE_RETRY_DELAYS_MS.len() - 1,
        ));
        let ctx = test_ctx(storage.clone(), "tid");
        let plan = sample_model_plan(plan_key.clone());
        let bytes = serde_json::to_vec_pretty(&plan).expect("plan to json");
        storage
            .inner
            .put_bytes(&plan_key, &bytes, "application/json")
            .await
            .expect("seed plan bytes");

        let loaded = load_model_plan_by_key(&ctx, &plan_key)
            .await
            .expect("transient read failures should be retried")
            .expect("plan should exist");

        assert_eq!(loaded.plan_key, plan_key);
        assert_eq!(storage.get_attempts(), SAFE_STORAGE_RETRY_DELAYS_MS.len());
    }
}
