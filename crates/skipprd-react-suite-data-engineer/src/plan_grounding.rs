use crate::naming;
use crate::plan_progress::{checklist_status, is_runnable_checklist_status, CHECKLIST_SQL_MODEL};
use crate::plan_types::*;
use crate::references::DatasetRef;
use react_core::agent::AgentCtx;

pub fn ensure_expected_model_paths_cleanse(ctx: Option<&AgentCtx>, plan: &mut CleansePlan) -> bool {
    let mut changed = false;
    for t in plan.tasks.iter_mut() {
        let Some(ds) = DatasetRef::parse(&t.dataset_id) else {
            continue;
        };
        let canonical = naming::canonical_staging_rel_path(&ds.schema, &ds.table);
        let cur = t
            .expected_model_path
            .as_deref()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if cur != canonical {
            changed = true;
            let from = if cur.is_empty() {
                "(missing)".to_string()
            } else {
                cur
            };
            let msg = format!(
                "canonicalized cleanse expected_model_path for {}: {} -> {}",
                t.dataset_id, from, canonical
            );
            tracing::warn!("{}", msg);
            if let Some(tx) = ctx.and_then(|c| c.trace_tx().as_ref()) {
                let _ = tx.send(msg);
            }
            t.expected_model_path = Some(canonical);
        }
    }
    changed
}

pub fn ensure_expected_model_paths_model(plan: &mut ModelPlan) {
    for t in plan.tasks.iter_mut() {
        let missing = t
            .expected_model_path
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        if !missing {
            continue;
        }
        t.expected_model_path = Some(format!(
            "models/{}/{}.sql",
            t.folder.as_str(),
            t.name.trim()
        ));
    }
}

pub fn prune_cleanse_plan_to_grounded_raw_datasets(
    plan: &mut CleansePlan,
    allowed_raw: &std::collections::BTreeSet<String>,
) {
    let mut removed: Vec<String> = Vec::new();
    plan.tasks.retain(|t| {
        let keep = allowed_raw.contains(t.dataset_id.trim());
        if !keep {
            removed.push(t.dataset_id.clone());
        }
        keep
    });

    for b in plan.batches.iter_mut() {
        b.retain(|ds| allowed_raw.contains(ds.trim()));
    }
    plan.batches.retain(|b| !b.is_empty());

    if !removed.is_empty() {
        removed.sort();
        removed.dedup();
        plan.project_snapshot.insert(
            "pruned_dataset_ids",
            serde_json::json!({
                "count": removed.len(),
                "items": removed.into_iter().take(50).collect::<Vec<_>>()
            }),
        );
    }
    plan.reconcile_work_groups();
}

pub fn prune_model_plan_to_grounded_staging_models(
    plan: &mut ModelPlan,
    allowed_stg_models: &std::collections::BTreeSet<String>,
) {
    fn normalize_staging_input_name(raw: &str) -> Option<String> {
        let mut t = raw.trim();
        if t.is_empty() {
            return None;
        }
        if t.starts_with("{{") && t.ends_with("}}") && t.len() >= 4 {
            t = t[2..t.len() - 2].trim();
        }
        let lower = t.to_ascii_lowercase();
        let mut candidate = if lower.starts_with("ref(") && t.ends_with(')') {
            let inner = &t[4..t.len() - 1];
            inner
                .trim()
                .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                .to_string()
        } else {
            t.to_string()
        };
        if candidate.contains('/') {
            let stem = std::path::Path::new(candidate.as_str())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !stem.trim().is_empty() {
                candidate = stem.trim().to_string();
            }
        }
        if candidate.contains('.') {
            if let Some(last) = candidate.rsplit('.').next() {
                candidate = last.trim().to_string();
            }
        }
        let normalized = candidate
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '`')
            .to_ascii_lowercase();
        if normalized.starts_with("stg_") {
            Some(normalized)
        } else {
            None
        }
    }

    let normalized_allowed: std::collections::BTreeSet<String> = allowed_stg_models
        .iter()
        .filter_map(|s| normalize_staging_input_name(s))
        .collect();

    let plan_task_names: std::collections::BTreeSet<String> = plan
        .tasks
        .iter()
        .map(|t| t.name.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut removed: Vec<String> = Vec::new();
    plan.tasks.retain_mut(|t| {
        let name = t.name.trim();
        if name.is_empty() {
            removed.push(t.name.clone());
            return false;
        }
        let has_any_input = t.inputs.iter().any(|inp| !inp.trim().is_empty());
        if !has_any_input {
            removed.push(t.name.clone());
            return false;
        }
        let mut normalized_inputs: Vec<String> = Vec::new();
        for inp in t.inputs.iter() {
            let it = inp.trim();
            if it.is_empty() {
                continue;
            }
            let Some(normalized) = normalize_staging_input_name(it) else {
                if plan_task_names.contains(it) {
                    normalized_inputs.push(it.to_string());
                    continue;
                }
                removed.push(t.name.clone());
                return false;
            };
            if !normalized_allowed.contains(&normalized) {
                removed.push(t.name.clone());
                return false;
            }
            normalized_inputs.push(normalized);
        }
        if normalized_inputs.is_empty() {
            removed.push(t.name.clone());
            return false;
        }
        normalized_inputs.sort();
        normalized_inputs.dedup();
        t.inputs = normalized_inputs;
        true
    });

    for b in plan.batches.iter_mut() {
        b.retain(|name| !name.trim().is_empty());
        b.retain(|name| plan.tasks.iter().any(|t| t.name == *name));
    }
    plan.batches.retain(|b| !b.is_empty());

    if !removed.is_empty() {
        removed.sort();
        removed.dedup();
        plan.project_snapshot.insert(
            "pruned_model_tasks",
            serde_json::json!({
                "count": removed.len(),
                "items": removed.into_iter().take(50).collect::<Vec<_>>()
            }),
        );
    }
    plan.reconcile_work_groups();
}

fn default_cleanse_implementation_spec() -> CleanseImplementationSpec {
    CleanseImplementationSpec {
        spec_version: 1,
        row_preserving: true,
        output_fields: vec![],
        prohibited_ops: vec![
            "no filtering".to_string(),
            "no dedup".to_string(),
            "no grain enforcement".to_string(),
        ],
    }
}

pub fn normalize_cleanse_plan_defaults(plan: &mut CleansePlan) {
    for t in plan.tasks.iter_mut() {
        let spec = t
            .implementation_spec
            .get_or_insert_with(default_cleanse_implementation_spec);
        spec.row_preserving = true;
        if spec.spec_version <= 0 {
            spec.spec_version = 1;
        }
        if spec.prohibited_ops.is_empty() {
            spec.prohibited_ops = vec![
                "no filtering".to_string(),
                "no dedup".to_string(),
                "no grain enforcement".to_string(),
            ];
        }

        let sql_status = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            let missing_path = t
                .expected_model_path
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if missing_path {
                if let Some(ds) = DatasetRef::parse(&t.dataset_id) {
                    t.expected_model_path = Some(crate::naming::canonical_staging_rel_path(
                        &ds.schema, &ds.table,
                    ));
                } else {
                    let safe = t
                        .dataset_id
                        .replace('.', "_")
                        .replace('/', "_")
                        .replace('\\', "_");
                    t.expected_model_path = Some(format!("models/staging/stg_{}.sql", safe));
                }
            }
        }
    }
}

/// Shared grounding errors for all plan types via `PlanTask`.
fn strict_grounding_errors<T: PlanTask>(plan: &Plan<T>) -> Vec<String> {
    let mut errors = Vec::new();
    for t in &plan.tasks {
        let tid = t.task_id();
        if tid.trim().is_empty() {
            errors.push("task id is empty".to_string());
            continue;
        }
        let missing_path = t
            .expected_model_path()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        if missing_path {
            errors.push(format!("{}: expected_model_path is required", tid));
        }
    }
    errors
}

fn strict_cleanse_grounding_errors(plan: &CleansePlan) -> Vec<String> {
    let mut errors = strict_grounding_errors(plan);
    for t in &plan.tasks {
        if t.dataset_id.trim().is_empty() {
            continue;
        }
        if t.implementation_spec.is_none() {
            errors.push(format!("{}: implementation_spec is required", t.dataset_id));
        }
        if t.source_schema.is_empty() {
            errors.push(format!(
                "{}: source_schema is required (catalog column context must be captured at enrichment time)",
                t.dataset_id
            ));
        }
    }
    errors
}

fn strict_model_grounding_errors(
    plan: &ModelPlan,
    allowed_staging_models: Option<&std::collections::BTreeSet<String>>,
) -> Vec<String> {
    let mut errors = strict_grounding_errors(plan);
    let plan_task_names: std::collections::BTreeSet<&str> = plan
        .tasks
        .iter()
        .map(|t| t.name.trim())
        .filter(|s| !s.is_empty())
        .collect();
    for t in &plan.tasks {
        if t.name.trim().is_empty() {
            continue;
        }
        if t.goal.trim().is_empty() {
            errors.push(format!("{}: goal is required", t.name));
        }
        let nonempty_inputs: Vec<String> = t
            .inputs
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if nonempty_inputs.is_empty() {
            errors.push(format!(
                "{}: at least one task.inputs item is required",
                t.name
            ));
        }
        let impl_inputs: Vec<String> = t
            .implementation_spec
            .as_ref()
            .map(|spec| {
                spec.inputs
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if impl_inputs.is_empty() {
            errors.push(format!(
                "{}: implementation_spec.inputs must be non-empty",
                t.name
            ));
        }
        if !impl_inputs.is_empty() && !nonempty_inputs.is_empty() {
            let left: std::collections::BTreeSet<String> =
                nonempty_inputs.iter().cloned().collect();
            let right: std::collections::BTreeSet<String> = impl_inputs.iter().cloned().collect();
            if left != right {
                errors.push(format!(
                    "{}: implementation_spec.inputs must match task.inputs exactly",
                    t.name
                ));
            }
        }
        if let Some(allowed) = allowed_staging_models {
            for inp in nonempty_inputs {
                if !allowed.contains(&inp) && !plan_task_names.contains(inp.as_str()) {
                    errors.push(format!(
                        "{}: input '{}' not grounded in known staging models or plan tasks",
                        t.name, inp
                    ));
                }
            }
        }
    }
    errors
}

impl TryFrom<CleansePlan> for GroundedCleansePlan {
    type Error = String;

    fn try_from(mut value: CleansePlan) -> Result<Self, Self::Error> {
        normalize_cleanse_plan_defaults(&mut value);
        let errors = strict_cleanse_grounding_errors(&value);
        if !errors.is_empty() {
            let mut errors = errors;
            errors.sort();
            errors.dedup();
            return Err(format!(
                "cleanse_plan_grounding_failed: {}",
                errors.join(" | ")
            ));
        }
        Ok(Self(value))
    }
}

#[cfg(test)]
impl TryFrom<CleansePlan> for PersistableCleansePlan {
    type Error = String;

    fn try_from(value: CleansePlan) -> Result<Self, Self::Error> {
        if value.status.is_terminal() {
            return Ok(Self::Terminal(value));
        }
        Ok(Self::Grounded(GroundedCleansePlan::try_from(value)?))
    }
}

impl GroundedModelPlan {
    /// Validate and ground a model plan against the known staging models.
    /// The allowlist is required — callers must provide the set of staging models
    /// that actually exist. This is the only way to construct a `GroundedModelPlan`.
    pub fn ground(
        mut plan: ModelPlan,
        allowed_staging_models: &std::collections::BTreeSet<String>,
    ) -> Result<Self, String> {
        ensure_expected_model_paths_model(&mut plan);
        let errors = strict_model_grounding_errors(&plan, Some(allowed_staging_models));
        if !errors.is_empty() {
            let mut errors = errors;
            errors.sort();
            errors.dedup();
            return Err(format!(
                "model_plan_grounding_failed: {}",
                errors.join(" | ")
            ));
        }
        Ok(Self(plan))
    }
}

#[cfg(test)]
impl PersistableModelPlan {
    pub fn from_plan(
        value: ModelPlan,
        allowed_staging_models: &std::collections::BTreeSet<String>,
    ) -> Result<Self, String> {
        if value.status.is_terminal() {
            return Ok(Self::Terminal(value));
        }
        Ok(Self::Grounded(GroundedModelPlan::ground(
            value,
            allowed_staging_models,
        )?))
    }
}
