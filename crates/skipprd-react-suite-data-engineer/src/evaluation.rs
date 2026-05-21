use crate::control_flow::Phase;
use crate::progress_controller::{PlanRevisionStrategy, PlanViolation};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationInput {
    pub phase: Phase,
    pub truth: crate::truth_snapshot::TruthSnapshot,
    pub plan: Option<ActivePlan>,
    pub implementation: DbtProjectSnapshot,
    pub evidence: Evidence,
    pub attempts: AttemptLedger,
}

pub(crate) async fn build_input(
    ctx: &react_core::agent::AgentCtx,
    phase: Phase,
    evidence: Evidence,
    execution_state: &crate::progress_controller::ExecutionState,
) -> Result<EvaluationInput, String> {
    Ok(EvaluationInput {
        phase,
        truth: crate::truth_snapshot::TruthSnapshot::build_for_phase(ctx, phase).await?,
        plan: ActivePlan::load_for_phase(ctx, phase).await?,
        implementation: DbtProjectSnapshot::build(ctx).await?,
        evidence,
        attempts: execution_state.attempt_ledger().clone(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum ActivePlan {
    Cleanse { plan_key: String },
    Model { plan_key: String },
}

impl ActivePlan {
    pub(crate) async fn load_for_phase(
        ctx: &react_core::agent::AgentCtx,
        phase: Phase,
    ) -> Result<Option<Self>, String> {
        if phase.tier() == crate::progress_controller::ExecutionTier::Cleanse {
            return Ok(crate::plan::load_cleanse_plan(ctx)
                .await
                .map_err(|e| e.to_string())?
                .map(|plan| Self::Cleanse {
                    plan_key: plan.plan_key,
                }));
        }
        if phase.tier() == crate::progress_controller::ExecutionTier::Model {
            return Ok(crate::plan::load_model_plan(ctx)
                .await
                .map_err(|e| e.to_string())?
                .map(|plan| Self::Model {
                    plan_key: plan.plan_key,
                }));
        }
        Ok(None)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbtProjectSnapshot {
    #[serde(default)]
    pub sql_models: BTreeMap<String, DbtModelFile>,
    #[serde(default)]
    pub schema_models: BTreeMap<String, DbtSchemaModel>,
    #[serde(default)]
    pub manifest_available: bool,
}

impl DbtProjectSnapshot {
    pub(crate) async fn build(ctx: &react_core::agent::AgentCtx) -> Result<Self, String> {
        let facts = crate::dbt_project_snapshot::scan_project_files(ctx).await?;
        Ok(Self {
            sql_models: facts
                .sql_models
                .into_iter()
                .map(|(name, model)| {
                    (
                        name,
                        DbtModelFile {
                            path: model.path,
                            model_name: model.model_name,
                            columns: model.columns,
                        },
                    )
                })
                .collect(),
            schema_models: facts
                .schema_models
                .into_iter()
                .map(|(key, model)| {
                    (
                        key,
                        DbtSchemaModel {
                            path: model.path,
                            model_name: model.model_name,
                            columns: model
                                .columns
                                .into_iter()
                                .map(|column| column.name)
                                .collect(),
                            in_staging_dir: model.in_staging_dir,
                        },
                    )
                })
                .collect(),
            manifest_available: facts.manifest_available,
        })
    }

    pub(crate) fn contract_drifts(&self) -> Vec<String> {
        let mut drifts = Vec::new();
        for (name, sql) in &self.sql_models {
            if sql.columns.is_empty() {
                continue;
            }
            let schema_entries = self
                .schema_models
                .values()
                .filter(|schema| schema.model_name == *name)
                .collect::<Vec<_>>();
            if schema_entries.is_empty() {
                drifts.push(format!(
                    "{name}: SQL model {} has no schema YAML entry",
                    sql.path
                ));
                continue;
            }
            for schema in schema_entries {
                if name.starts_with("stg_") && !schema.in_staging_dir {
                    drifts.push(format!(
                        "{name}: staging model schema is declared in {}; expected models/staging/*.yml",
                        schema.path
                    ));
                }
                let mut sql_columns = sql.columns.clone();
                sql_columns.sort();
                sql_columns.dedup();
                let mut yaml_columns = schema.columns.clone();
                yaml_columns.sort();
                yaml_columns.dedup();
                if sql_columns != yaml_columns {
                    drifts.push(format!(
                        "{name}: schema columns in {} do not match SQL output columns in {} (sql=[{}], yaml=[{}])",
                        schema.path,
                        sql.path,
                        sql_columns.join(", "),
                        yaml_columns.join(", ")
                    ));
                }
            }
        }
        drifts
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbtModelFile {
    pub path: String,
    pub model_name: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbtSchemaModel {
    pub path: String,
    pub model_name: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub in_staging_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "source")]
pub(crate) enum Evidence {
    None,
    DbtValidate(DbtValidateEvidence),
    DbtValidateExecution { error: String },
    SchemaPrecheck(PrecheckEvidence),
    AuthoringBatch(BatchEvidence),
    RuntimePrerequisite(RuntimePrerequisiteEvidence),
    TruthIncomplete(TruthIncompleteEvidence),
    PlanSemantic(PlanSemanticEvidence),
    Review(ReviewEvidence),
    Contract { drifts: Vec<String> },
}

impl Evidence {
    pub(crate) fn brief(&self) -> String {
        match self {
            Self::None => "no evidence".to_string(),
            Self::DbtValidate(e) => e.brief.clone(),
            Self::DbtValidateExecution { error } => error.clone(),
            Self::SchemaPrecheck(e) => e.brief.clone(),
            Self::AuthoringBatch(e) => e.brief.clone(),
            Self::RuntimePrerequisite(e) => e.brief.clone(),
            Self::TruthIncomplete(e) => e.brief.clone(),
            Self::PlanSemantic(e) => e.brief.clone(),
            Self::Review(e) => e.brief.clone(),
            Self::Contract { drifts } => {
                if drifts.is_empty() {
                    "contract drift".to_string()
                } else {
                    drifts.join("\n")
                }
            }
        }
    }

    pub(crate) fn log_excerpts(&self) -> Option<String> {
        match self {
            Self::DbtValidate(e) => e.log_excerpts.clone(),
            Self::SchemaPrecheck(e) => e.log_excerpts.clone(),
            Self::RuntimePrerequisite(e) => e.log_excerpts.clone(),
            Self::TruthIncomplete(e) => e.log_excerpts.clone(),
            Self::PlanSemantic(e) => e.log_excerpts.clone(),
            _ => None,
        }
    }

    pub(crate) fn hash(&self) -> String {
        let serialized = serde_json::to_string(self).unwrap_or_else(|_| self.brief());
        react_core::llm_observability::sha256_hex_str(&serialized)
    }

    pub(crate) fn repair_context(&self) -> RepairEvidenceContext {
        let compile_ok = match self {
            Self::DbtValidate(e) => e.compile_ok,
            Self::Review(_) => true,
            _ => false,
        };
        let run_ok = match self {
            Self::DbtValidate(e) => e.run_ok,
            Self::Review(_) => true,
            _ => false,
        };
        RepairEvidenceContext {
            brief: self.brief(),
            log_excerpts: self.log_excerpts(),
            compile_ok,
            run_ok,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepairEvidenceContext {
    pub brief: String,
    pub log_excerpts: Option<String>,
    pub compile_ok: bool,
    pub run_ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbtValidateEvidence {
    pub brief: String,
    pub failure_hash: String,
    pub compile_ok: bool,
    pub run_ok: bool,
    pub log_excerpts: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrecheckEvidence {
    pub brief: String,
    pub log_excerpts: Option<String>,
    #[serde(default)]
    pub suggested_targets: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchEvidence {
    pub brief: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimePrerequisiteEvidence {
    pub brief: String,
    pub log_excerpts: Option<String>,
    #[serde(default)]
    pub resources: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TruthIncompleteEvidence {
    pub brief: String,
    pub log_excerpts: Option<String>,
    #[serde(default)]
    pub missing_inputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanSemanticEvidence {
    pub brief: String,
    pub log_excerpts: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewEvidence {
    pub brief: String,
    #[serde(default)]
    pub target_task_ids: Vec<String>,
    #[serde(default)]
    pub requests_plan_change: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub(crate) enum EvaluationVerdict {
    Accepted,
    RepairImplementation {
        evidence: Evidence,
    },
    RevisePlan {
        violations: Vec<PlanViolation>,
        strategy: PlanRevisionStrategy,
    },
    RefreshTruth {
        reason: String,
    },
    Fatal {
        reason: String,
    },
}

impl EvaluationVerdict {
    pub(crate) fn kind(&self) -> VerdictKind {
        match self {
            Self::Accepted => VerdictKind::Accepted,
            Self::RepairImplementation { .. } => VerdictKind::RepairImplementation,
            Self::RevisePlan { .. } => VerdictKind::RevisePlan,
            Self::RefreshTruth { .. } => VerdictKind::RefreshTruth,
            Self::Fatal { .. } => VerdictKind::Fatal,
        }
    }

    pub(crate) fn summary(&self) -> EvaluationVerdictSummary {
        match self {
            Self::Accepted => EvaluationVerdictSummary {
                kind: VerdictKind::Accepted,
                message: "accepted".to_string(),
            },
            Self::RepairImplementation { evidence } => EvaluationVerdictSummary {
                kind: VerdictKind::RepairImplementation,
                message: evidence.brief(),
            },
            Self::RevisePlan { violations, .. } => EvaluationVerdictSummary {
                kind: VerdictKind::RevisePlan,
                message: crate::progress_controller::format_plan_violations(violations),
            },
            Self::RefreshTruth { reason } => EvaluationVerdictSummary {
                kind: VerdictKind::RefreshTruth,
                message: reason.clone(),
            },
            Self::Fatal { reason } => EvaluationVerdictSummary {
                kind: VerdictKind::Fatal,
                message: reason.clone(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Accepted,
    RepairImplementation,
    RevisePlan,
    RefreshTruth,
    Fatal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvaluationVerdictSummary {
    pub kind: VerdictKind,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptLedger {
    #[serde(default)]
    attempts: BTreeMap<String, usize>,
}

impl AttemptLedger {
    pub(crate) fn count(&self, key: &AttemptKey) -> usize {
        self.attempts.get(&key.storage_key()).copied().unwrap_or(0)
    }

    pub(crate) fn record(&mut self, key: AttemptKey) -> usize {
        let count = self.attempts.entry(key.storage_key()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub struct AttemptKey {
    pub phase: String,
    pub verdict_kind: VerdictKind,
    pub evidence_hash: String,
}

impl AttemptKey {
    pub(crate) fn storage_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.phase.trim(),
            serde_json::to_string(&self.verdict_kind)
                .unwrap_or_else(|_| format!("{:?}", self.verdict_kind))
                .trim_matches('"'),
            self.evidence_hash.trim()
        )
    }
}

pub(crate) const DEFAULT_ATTEMPT_CAP: usize = 3;

pub(crate) fn classify_infra_errors(errors: &[String]) -> crate::failure_kind::FailureKind {
    crate::failure_text::classify_dbt_error_kind(errors)
}

pub(crate) fn classify_execution_error(error: &str) -> crate::failure_kind::FailureKind {
    let normalized = crate::failure_text::normalize_text(error);
    if crate::failure_text::matches_infra_transient(&normalized) {
        crate::failure_kind::FailureKind::InfraTransient
    } else if crate::failure_text::is_infra_config(&normalized) {
        crate::failure_kind::FailureKind::InfraConfig
    } else {
        crate::failure_kind::FailureKind::Unknown
    }
}

pub(crate) fn evaluate(input: EvaluationInput) -> EvaluationVerdict {
    if matches!(&input.evidence, Evidence::None | Evidence::DbtValidate(_)) {
        let drifts = input.implementation.contract_drifts();
        if !drifts.is_empty() {
            return EvaluationVerdict::RepairImplementation {
                evidence: Evidence::Contract { drifts },
            };
        }
    }
    match &input.evidence {
        Evidence::None => EvaluationVerdict::Accepted,
        Evidence::DbtValidate(evidence) => {
            match classify_infra_errors(&evidence.errors) {
                crate::failure_kind::FailureKind::InfraTransient => {
                    EvaluationVerdict::Fatal {
                        reason: format!(
                            "dbt_validate failed due to a transient infrastructure error. Retry after the upstream service recovers.\n\n{}",
                            evidence.brief
                        ),
                    }
                }
                crate::failure_kind::FailureKind::InfraConfig => EvaluationVerdict::Fatal {
                    reason: format!(
                        "dbt_validate failed due to an environment/configuration error. Fix credentials, profiles, or auth configuration before retrying.\n\n{}",
                        evidence.brief
                    ),
                },
                crate::failure_kind::FailureKind::Unknown => EvaluationVerdict::RepairImplementation {
                    evidence: input.evidence,
                },
            }
        }
        Evidence::DbtValidateExecution { error } => match classify_execution_error(error) {
            crate::failure_kind::FailureKind::InfraTransient => EvaluationVerdict::Fatal {
                reason: format!(
                    "dbt_validate execution failed due to transient infrastructure error: {error}"
                ),
            },
            crate::failure_kind::FailureKind::InfraConfig => EvaluationVerdict::Fatal {
                reason: format!("dbt_validate execution failed due to configuration error: {error}"),
            },
            crate::failure_kind::FailureKind::Unknown => EvaluationVerdict::RepairImplementation {
                evidence: input.evidence,
            },
        },
        Evidence::SchemaPrecheck(_) | Evidence::AuthoringBatch(_) | Evidence::Contract { .. } => {
            EvaluationVerdict::RepairImplementation {
                evidence: input.evidence,
            }
        }
        Evidence::RuntimePrerequisite(evidence) => EvaluationVerdict::Fatal {
            reason: evidence.brief.clone(),
        },
        Evidence::TruthIncomplete(evidence) => EvaluationVerdict::RefreshTruth {
            reason: evidence.brief.clone(),
        },
        Evidence::PlanSemantic(evidence) => {
            let violations = plan_semantic_violations(input.phase, &evidence.errors, &evidence.brief);
            let can_amend = input.plan.is_some()
                && !violations.is_empty()
                && violations
                    .iter()
                    .all(|violation| violation.task_id.is_some());
            EvaluationVerdict::RevisePlan {
                violations,
                strategy: if can_amend {
                    PlanRevisionStrategy::Amend
                } else {
                    PlanRevisionStrategy::Rewrite
                },
            }
        }
        Evidence::Review(review) if review.requests_plan_change => {
            let violations = if review.target_task_ids.is_empty() {
                vec![PlanViolation::new(
                    input.phase,
                    None,
                    format!("Review requested plan change: {}", review.brief),
                )]
            } else {
                review
                    .target_task_ids
                    .iter()
                    .map(|id| {
                        PlanViolation::new(
                            input.phase,
                            Some(id.trim().to_string()),
                            format!("Review requested plan change: {}", review.brief),
                        )
                    })
                    .collect()
            };
            EvaluationVerdict::RevisePlan {
                violations,
                strategy: if review.target_task_ids.is_empty() {
                    PlanRevisionStrategy::Rewrite
                } else {
                    PlanRevisionStrategy::Amend
                },
            }
        }
        Evidence::Review(_) => EvaluationVerdict::RepairImplementation {
            evidence: input.evidence,
        },
    }
}

fn plan_semantic_violations(phase: Phase, errors: &[String], brief: &str) -> Vec<PlanViolation> {
    if errors.is_empty() {
        return vec![PlanViolation::new(phase, None, brief.to_string())];
    }
    errors
        .iter()
        .map(|error| {
            let task_id = error
                .split_once(':')
                .map(|(head, _)| head.trim())
                .filter(|head| {
                    !head.is_empty()
                        && head
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                })
                .map(|head| head.to_string());
            PlanViolation::new(phase, task_id, error.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_input(evidence: Evidence) -> EvaluationInput {
        EvaluationInput {
            phase: Phase::ModelValidate,
            truth: crate::truth_snapshot::TruthSnapshot::default(),
            plan: None,
            implementation: DbtProjectSnapshot::default(),
            evidence,
            attempts: AttemptLedger::default(),
        }
    }

    #[test]
    fn evaluate_accepts_no_evidence() {
        assert!(matches!(
            evaluate(empty_input(Evidence::None)),
            EvaluationVerdict::Accepted
        ));
    }

    #[test]
    fn validate_sql_error_routes_to_repair() {
        let verdict = evaluate(empty_input(Evidence::DbtValidate(DbtValidateEvidence {
            brief: "syntax error".to_string(),
            failure_hash: "h".to_string(),
            compile_ok: false,
            run_ok: false,
            log_excerpts: None,
            errors: vec!["syntax error at or near FROM".to_string()],
        })));
        assert!(matches!(
            verdict,
            EvaluationVerdict::RepairImplementation { .. }
        ));
    }

    #[test]
    fn validate_config_error_routes_fatal() {
        let verdict = evaluate(empty_input(Evidence::DbtValidate(DbtValidateEvidence {
            brief: "auth denied".to_string(),
            failure_hash: "h".to_string(),
            compile_ok: false,
            run_ok: false,
            log_excerpts: None,
            errors: vec!["accessdenied: user is not authorized".to_string()],
        })));
        assert!(matches!(verdict, EvaluationVerdict::Fatal { .. }));
    }

    #[test]
    fn review_plan_change_routes_to_revision() {
        let verdict = evaluate(empty_input(Evidence::Review(ReviewEvidence {
            brief: "wrong grain".to_string(),
            target_task_ids: vec!["fct_orders".to_string()],
            requests_plan_change: true,
        })));
        assert!(matches!(verdict, EvaluationVerdict::RevisePlan { .. }));
    }

    #[test]
    fn truth_incomplete_routes_to_refresh_truth() {
        let verdict = evaluate(empty_input(Evidence::TruthIncomplete(
            TruthIncompleteEvidence {
                brief: "staging columns unavailable".to_string(),
                log_excerpts: None,
                missing_inputs: vec!["stg_raw_bike_hire".to_string()],
            },
        )));
        assert!(matches!(verdict, EvaluationVerdict::RefreshTruth { .. }));
    }

    #[test]
    fn plan_semantic_routes_to_plan_revision() {
        let verdict = evaluate(empty_input(Evidence::PlanSemantic(PlanSemanticEvidence {
            brief: "plan references fields absent from truth".to_string(),
            log_excerpts: None,
            errors: vec!["fct_rides: missing field EVENT_DATE".to_string()],
        })));
        assert!(matches!(verdict, EvaluationVerdict::RevisePlan { .. }));
    }

    #[test]
    fn attempt_ledger_counts_by_evidence_hash() {
        let mut ledger = AttemptLedger::default();
        let key = AttemptKey {
            phase: Phase::ModelValidate.as_str().to_string(),
            verdict_kind: VerdictKind::RepairImplementation,
            evidence_hash: "abc".to_string(),
        };
        assert_eq!(ledger.record(key.clone()), 1);
        assert_eq!(ledger.record(key.clone()), 2);
        assert_eq!(ledger.count(&key), 2);
    }

    #[test]
    fn attempt_ledger_serializes_as_json_object() {
        let mut ledger = AttemptLedger::default();
        let key = AttemptKey {
            phase: Phase::CleanseValidate.as_str().to_string(),
            verdict_kind: VerdictKind::RepairImplementation,
            evidence_hash: "abc".to_string(),
        };
        ledger.record(key);
        let value = serde_json::to_value(&ledger).expect("ledger should serialize");
        assert!(value["attempts"].is_object());
    }
}
