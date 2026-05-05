use crate::failure_kind::FailureKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

use crate::domain_types::ReviewDecisionMeta;
use react_core::session::ControlStateStore;

use crate::control_flow::Phase;

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 2;
pub const MAX_REPAIR_CYCLES: usize = 8;

// ── New typed enums (state machine hard cutover) ──

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum RepairStatus {
    Idle,
    Pending { cycle: usize },
    Exhausted { cycles_used: usize },
}

impl Default for RepairStatus {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ValidationFailureContext {
    pub brief: String,
    pub log_excerpts: Option<String>,
    pub compile_ok: bool,
    pub run_ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PublishStatus {
    NotRequested,
    PlanPending {
        plan_sha256: String,
    },
    AwaitingApproval {
        plan_sha256: Option<String>,
        #[serde(default)]
        approval_retries: usize,
    },
    Approved {
        plan_sha256: Option<String>,
    },
    Succeeded {
        plan_sha256: String,
    },
    Failed {
        #[serde(default)]
        publish_retries: usize,
    },
}

impl Default for PublishStatus {
    fn default() -> Self {
        Self::NotRequested
    }
}

impl PublishStatus {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved { .. })
    }

    pub fn retry_count_for_kind(&self, kind: PublishRetryKind) -> usize {
        match (self, kind) {
            (
                Self::AwaitingApproval {
                    approval_retries, ..
                },
                PublishRetryKind::AwaitApprovalLoop,
            ) => *approval_retries,
            (Self::Failed { publish_retries }, PublishRetryKind::PublishFailureLoop) => {
                *publish_retries
            }
            _ => 0,
        }
    }

    pub fn plan_sha256(&self) -> Option<&str> {
        match self {
            Self::PlanPending { plan_sha256 } => Some(plan_sha256),
            Self::AwaitingApproval { plan_sha256, .. } => plan_sha256.as_deref(),
            Self::Approved { plan_sha256 } => plan_sha256.as_deref(),
            Self::Succeeded { plan_sha256 } => Some(plan_sha256),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ProbeStatus {
    NotRequired,
    Required { attempts: ProbeAttempts },
    Satisfied { attempts: ProbeAttempts },
    ExhaustedRequireMutation { attempts: ProbeAttempts },
}

impl Default for ProbeStatus {
    fn default() -> Self {
        Self::NotRequired
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProbeAttempts {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub meaningful: usize,
    #[serde(default)]
    pub non_meaningful: usize,
    #[serde(default)]
    pub failed: usize,
    #[serde(default)]
    pub repeated_signature_streak: usize,
    #[serde(default)]
    pub last_signature: Option<ProbeSignature>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "transition")]
pub enum PhaseTransition {
    PreflightOk {
        dbt_project_key: String,
        has_query_provider: bool,
        has_dbt_provider: bool,
    },
    PlanAutoApproved {
        source: AutoApprovalSource,
    },
    PlanAlreadyApproved,
    PlanMissing {
        kind: TrackKind,
        note: String,
    },
    PlanNotApproved {
        status: PlanStatus,
    },
    PlanInvalidEmpty {
        plan_key: String,
        tasks_len: usize,
        batches_len: usize,
    },
    PlanPrunedEmpty {
        plan_key: String,
    },
    PlanSemanticInvalid {
        plan_key: String,
        errors: Vec<String>,
    },
    PlanRevisionRequested {
        violations: Vec<PlanViolation>,
        strategy: PlanRevisionStrategy,
    },
    ValidatePassToAuthoring {
        signal: String,
        plan_key: Option<String>,
        pending_count: usize,
    },
    ValidatePassToReview {
        step_idx: usize,
    },
    ValidateFail {
        errors: Vec<String>,
    },
    PrecheckFailed {
        reason: String,
    },
    ValidateExecutionFailed {
        reason: String,
    },
    ValidateContractError {
        reason: String,
    },
    ReviewProceed,
    ReviewPatchImpl {
        meta: ReviewDecisionMeta,
        target_task_ids: Vec<String>,
    },
    ReviewProjectSummary,
    ReviewBatch,
    ReviewFinalUnify,
    WorkGroupValidate,
    PlanTasksDone,
    NoWorkAllDone,
    AuthoringComplete,
    PublishApproved,
    PublishSuccess,
    PublishFail {
        retry_count: usize,
    },
    PublishConfirmedSuccess,
    PublishConfirmedFail,
    PhaseBlocked,
    RepairCompleted,
    RepairExhausted {
        reason: String,
    },

    ElDiscoverOk {
        namespaces_count: usize,
    },
    ElSyncOk {
        tables_synced: usize,
    },
    ElSyncFailed {
        reason: String,
    },
    ElVerifyOk,
    ElVerifyFailed {
        reason: String,
    },
}

impl PhaseTransition {
    pub fn as_reason_str(&self) -> &'static str {
        match self {
            Self::PreflightOk { .. } => "preflight_ok",
            Self::PlanAutoApproved { .. } => "plan_auto_approved",
            Self::PlanAlreadyApproved => "plan_already_approved",
            Self::PlanMissing { .. } => "plan_missing",
            Self::PlanNotApproved { .. } => "plan_not_approved",
            Self::PlanInvalidEmpty { .. } => "plan_invalid_empty",
            Self::PlanPrunedEmpty { .. } => "plan_pruned_empty",
            Self::PlanSemanticInvalid { .. } => "plan_semantic_invalid",
            Self::PlanRevisionRequested { .. } => "plan_revision_requested",
            Self::ValidatePassToAuthoring { .. } => "validate_pass_to_authoring",
            Self::ValidatePassToReview { .. } => "validate_pass_to_review",
            Self::ValidateFail { .. } => "validate_fail",
            Self::PrecheckFailed { .. } => "precheck_failed",
            Self::ValidateExecutionFailed { .. } => "validate_execution_failed",
            Self::ValidateContractError { .. } => "validate_contract_error",
            Self::ReviewProceed => "review_proceed",
            Self::ReviewPatchImpl { .. } => "review_patch_impl",
            Self::ReviewProjectSummary => "review_project_summary",
            Self::ReviewBatch => "review_batch",
            Self::ReviewFinalUnify => "review_final_unify",
            Self::WorkGroupValidate => "work_group_validate",
            Self::PlanTasksDone => "plan_tasks_done",
            Self::NoWorkAllDone => "no_work_all_done",
            Self::AuthoringComplete => "authoring_complete",
            Self::PublishApproved => "user_approved_publish",
            Self::PublishSuccess => "publish_success",
            Self::PublishFail { .. } => "publish_fail",
            Self::PublishConfirmedSuccess => "publish_confirmed_success",
            Self::PublishConfirmedFail => "publish_confirmed_fail",
            Self::PhaseBlocked => "phase_blocked",
            Self::RepairCompleted => "repair_completed",
            Self::RepairExhausted { .. } => "repair_exhausted",
            Self::ElDiscoverOk { .. } => "el_discover_ok",
            Self::ElSyncOk { .. } => "el_sync_ok",
            Self::ElSyncFailed { .. } => "el_sync_failed",
            Self::ElVerifyOk => "el_verify_ok",
            Self::ElVerifyFailed { .. } => "el_verify_failed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AutoApprovalSource {
    UserConfig,
    SystemDefault,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Cleanse,
    Model,
}

impl TrackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cleanse => "cleanse",
            Self::Model => "model",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    InProgress,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepairContext {
    pub failure: Option<ValidationFailureContext>,
    pub status: RepairStatus,
    pub recent_failed_file_ops: Vec<RecentFailedFileOp>,
}

impl RepairContext {
    pub fn brief(&self) -> Option<&str> {
        self.failure.as_ref().map(|f| f.brief.as_str())
    }

    pub fn log_excerpts(&self) -> Option<&str> {
        self.failure
            .as_ref()
            .and_then(|f| f.log_excerpts.as_deref())
    }

    pub fn repair_cycles(&self) -> usize {
        match &self.status {
            RepairStatus::Idle => 0,
            RepairStatus::Pending { cycle } => *cycle,
            RepairStatus::Exhausted { cycles_used } => *cycles_used,
        }
    }

    pub fn has_context(&self) -> bool {
        self.failure.is_some() || !self.recent_failed_file_ops.is_empty()
    }

    pub fn format_error_context(&self) -> String {
        let mut out = String::new();
        if let Some(brief) = self.brief() {
            let trimmed = brief.trim();
            if !trimmed.is_empty() {
                out.push_str("Last dbt_validate summary:\n");
                out.push_str(trimmed);
                out.push('\n');
            }
        }
        if !self.recent_failed_file_ops.is_empty() {
            out.push_str("\nRecent failed file mutations (do NOT repeat these verbatim):\n");
            let mut total_failure_count: usize = 0;
            for item in self.recent_failed_file_ops.iter().take(8) {
                total_failure_count += item.count;
                if item.count > 1 {
                    out.push_str(&format!(
                        "- file op='{}' path='{}' failed {} times: {}\n",
                        item.op, item.path, item.count, item.error_brief
                    ));
                } else {
                    out.push_str(&format!(
                        "- file op='{}' path='{}' failed: {}\n",
                        item.op, item.path, item.error_brief
                    ));
                }
            }
            out.push_str(
                "If a prior patch failed, read the exact current file content and make a materially different edit.\n",
            );
            for item in self.recent_failed_file_ops.iter().take(8) {
                if item.count >= 3 {
                    out.push_str(&format!(
                        "\nBLOCKED: file op='{}' path='{}' has failed {} times with the same error. \
                         You MUST NOT attempt the same operation again. Re-read the error and take a completely different approach \
                         (e.g. rename the file with op=mv, change the source reference, or use op=write instead of op=patch).\n",
                        item.op, item.path, item.count
                    ));
                }
            }
            if total_failure_count >= 3 {
                out.push_str(&format!(
                    "\nYou have {} recent failed file mutations. You MUST take a fundamentally different approach.\n\
                     If patching a YAML file keeps failing, consider using op=write with the complete correct file content.\n\
                     If the same path validation error repeats, re-read the error and change your approach entirely.\n",
                    total_failure_count
                ));
            }
        }
        let has_repair_data = self.failure.is_some() || !self.recent_failed_file_ops.is_empty();
        if has_repair_data {
            out.push_str(
                "\nCRITICAL REPAIR RULES:\n\
                 - You MUST change the SQL logic or test definition to fix the actual error described above.\n\
                 - Read the error message carefully: identify the failing column/expression, then edit \
                 the SQL model to produce correct values (filter NULLs, fix joins, cast types, etc.).\n\
                 - Use your tools (file list, file read) to identify which model(s) or test(s) need fixing.\n",
            );
        }
        if self.repair_cycles() >= 2 {
            out.push_str(&format!(
                "\nWARNING: Repair cycle {} of {}. Previous attempts did NOT fully resolve the issue. \
                 You MUST take a materially different approach.\n",
                self.repair_cycles(), MAX_REPAIR_CYCLES,
            ));
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentFailedFileOp {
    pub op: String,
    pub path: String,
    pub error_brief: String,
    pub count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishPlanState {
    #[serde(default)]
    pub pending_plan_sha256: Option<String>,
    #[serde(default)]
    pub pending_set_ts: Option<String>,
    #[serde(default)]
    pub last_published_plan_sha256: Option<String>,
    #[serde(default)]
    pub published_ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFocusState {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub dataset_id: Option<String>,
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LastMutationSummary {
    pub op: MutationOp,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub select_terms: Vec<String>,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MutationOp {
    Patch,
    Move,
    Remove,
}

impl Default for MutationOp {
    fn default() -> Self {
        Self::Patch
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTier {
    Unknown,
    El,
    Cleanse,
    Model,
}

impl Default for ExecutionTier {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct SqlModelPath(String);

impl SqlModelPath {
    pub fn parse(path: impl Into<String>) -> Result<Self, String> {
        let path = path.into();
        if !is_sql_model_path(path.as_str()) {
            return Err(format!(
                "invalid SqlModelPath '{}': expected models/*.sql path",
                path
            ));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SqlModelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl TryFrom<String> for SqlModelPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SqlModelPath> for String {
    fn from(value: SqlModelPath) -> Self {
        value.0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct SchemaDocPath(String);

impl SchemaDocPath {
    pub fn parse(path: impl Into<String>) -> Result<Self, String> {
        let path = path.into();
        if !is_schema_doc_path(path.as_str()) {
            return Err(format!(
                "invalid SchemaDocPath '{}': expected models/*.yml|yaml path",
                path
            ));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<String> for SchemaDocPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SchemaDocPath> for String {
    fn from(value: SchemaDocPath) -> Self {
        value.0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub enum RepairTargetPath {
    SqlModel(SqlModelPath),
    SchemaDoc(SchemaDocPath),
}

impl RepairTargetPath {
    pub fn parse(path: impl Into<String>) -> Result<Self, String> {
        let path = path.into();
        if let Ok(sql) = SqlModelPath::parse(path.clone()) {
            return Ok(Self::SqlModel(sql));
        }
        if let Ok(schema) = SchemaDocPath::parse(path.clone()) {
            return Ok(Self::SchemaDoc(schema));
        }
        Err(format!(
            "invalid RepairTargetPath '{}': expected models/*.sql or models/*.yml|yaml path",
            path
        ))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::SqlModel(path) => path.as_str(),
            Self::SchemaDoc(path) => path.as_str(),
        }
    }
}

impl fmt::Display for RepairTargetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for RepairTargetPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RepairTargetPath> for String {
    fn from(value: RepairTargetPath) -> Self {
        match value {
            RepairTargetPath::SqlModel(path) => path.into(),
            RepairTargetPath::SchemaDoc(path) => path.into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcomeKind {
    MeaningfulNewSignal,
    MeaningfulSameSignal,
    NonMeaningful,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeRequirementStatus {
    NotRequired,
    Required,
    Allowed,
    ExhaustedRequireMutation,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProbeSignature {
    #[serde(default)]
    pub normalized_sql: String,
    #[serde(default)]
    pub row_count: usize,
    #[serde(default)]
    pub header_count: usize,
    #[serde(default)]
    pub first_row_fingerprint: Option<String>,
}

impl ProbeSignature {
    pub fn from_run_sql(sql: &str, observation: &Value) -> Self {
        if let Some(p) = observation.get("probe") {
            let normalized_sql = p
                .get("normalized_sql")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    sql.split_whitespace()
                        .collect::<Vec<&str>>()
                        .join(" ")
                        .to_ascii_lowercase()
                });
            let row_count = p
                .get("row_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0);
            let header_count = p
                .get("header_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0);
            let first_row_fingerprint = p
                .get("first_row_fingerprint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Self {
                normalized_sql,
                row_count,
                header_count,
                first_row_fingerprint,
            };
        }
        let normalized_sql = sql
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ")
            .to_ascii_lowercase();
        let header_count = observation
            .get("header")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let rows = observation
            .get("rows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let row_count = rows.len();
        let first_row_fingerprint = rows.first().and_then(|row| {
            row.as_array().map(|arr| {
                let preview: Vec<&Value> = arr.iter().take(6).collect();
                serde_json::to_string(&preview).unwrap_or_default()
            })
        });
        Self {
            normalized_sql,
            row_count,
            header_count,
            first_row_fingerprint,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SubjectiveRetryKind {
    PlanSemanticInvalid,
    PlanGroundingEmptyAfterPrune,
    PlanGroundingStagingDiscoveryEmpty,
    ReviewPatchImpl,
    ReviewPlanChange,
    ValidatePrecheckFailed,
    ValidateExecutionFailed,
    ValidateFailedRetry,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PublishApprovalDecision {
    AwaitingUserApproval,
    Approved,
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishApprovalState {
    pub decision: PublishApprovalDecision,
    pub ts: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PublishRetryKind {
    AwaitApprovalLoop,
    PublishFailureLoop,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishRetryState {
    pub kind: PublishRetryKind,
    pub count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PlanViolation {
    pub detecting_phase: Phase,
    pub task_id: Option<String>,
    pub evidence: String,
}

impl PlanViolation {
    pub fn new(
        detecting_phase: Phase,
        task_id: Option<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            detecting_phase,
            task_id,
            evidence: evidence.into(),
        }
    }
}

pub fn format_plan_violations(violations: &[PlanViolation]) -> String {
    if violations.is_empty() {
        return String::new();
    }
    let mut out = String::from("PLAN REVISION REQUIRED — downstream phases reported the following issues with the current plan:\n\n");
    for (i, v) in violations.iter().enumerate() {
        out.push_str(&format!("Issue {}:\n", i + 1));
        out.push_str(&format!("  Detected by: {}\n", v.detecting_phase.as_str()));
        if let Some(ref tid) = v.task_id {
            out.push_str(&format!("  Plan task: {}\n", tid));
        }
        out.push_str(&format!("  Evidence: {}\n\n", v.evidence.trim()));
    }
    out.push_str(
        "Revise the plan to fix these issues. Do NOT repeat the same unachievable instructions. \
For each affected existing file, add concrete fix directives in the task checklist/details (for example: remove the stale field, preserve nulls instead of filtering, rename the output column, or restore the planned input).\n",
    );
    out
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PatchImplIntent {
    pub phase: Phase,
    #[serde(default)]
    pub mutated_since_set: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanRevisionIntent {
    pub violations: Vec<PlanViolation>,
    #[serde(default)]
    pub strategy: PlanRevisionStrategy,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanRevisionStrategy {
    Rewrite,
    Amend,
}

impl Default for PlanRevisionStrategy {
    fn default() -> Self {
        Self::Rewrite
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLookupPathKind {
    CanonicalTarget,
    Ambiguous,
    NonCanonical,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLookupFailureKind {
    NoSuchKey,
    PointerNotFound,
}

impl ManifestLookupFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchKey => "NoSuchKey",
            Self::PointerNotFound => "PointerNotFound",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestLookupState {
    #[serde(default)]
    pub retry_suppressed: bool,
    #[serde(default)]
    pub failure_signature: Option<String>,
    #[serde(default)]
    pub repeated_failure_count: usize,
    #[serde(default)]
    pub canonical_success_count: usize,
    #[serde(default)]
    pub noncanonical_attempt_count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionState {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) phase: PhaseState,
    #[serde(default)]
    pub(crate) repair: RepairState,
    #[serde(default)]
    pub(crate) publish: PublishStatus,
    #[serde(default)]
    pub(crate) manifest: ManifestState,
    #[serde(default)]
    pub(crate) telemetry: TelemetryState,
    #[serde(default)]
    pub(crate) subjective_retries: BTreeMap<SubjectiveRetryKind, usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelemetryState {
    #[serde(default)]
    pub probe: ProbeStatus,
    #[serde(default)]
    pub artifact_focus: Option<ArtifactFocusState>,
    #[serde(default)]
    pub last_mutation_summary: Option<LastMutationSummary>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PhaseState {
    #[serde(default)]
    pub current_phase: Phase,
    #[serde(default)]
    pub transition: Option<PhaseTransition>,
    #[serde(default)]
    pub replan_backtracks: usize,
    #[serde(default)]
    pub pending_plan_revision: Option<PlanRevisionIntent>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RepairState {
    #[serde(default)]
    pub status: RepairStatus,
    #[serde(default)]
    pub failure_context: Option<ValidationFailureContext>,
    #[serde(default)]
    pub last_failure_hash: Option<String>,
    #[serde(default)]
    pub mutated_since_fail: bool,
    #[serde(default)]
    pub pending_patch_impl: Option<PatchImplIntent>,
    #[serde(default)]
    pub infra_transient: bool,
}

impl RepairState {
    pub fn hard_mutation_repair_mode(&self) -> bool {
        matches!(self.status, RepairStatus::Pending { .. })
    }

    pub fn cycle_count(&self) -> usize {
        match &self.status {
            RepairStatus::Idle => 0,
            RepairStatus::Pending { cycle } => *cycle,
            RepairStatus::Exhausted { cycles_used } => *cycles_used,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestState {
    #[serde(default)]
    pub manifest_lookup: ManifestLookupState,
    #[serde(default)]
    pub cleanse_plan_bootstrapped: bool,
    #[serde(default)]
    pub model_plan_bootstrapped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DataEngineerEvent {
    ValidatePassed {
        tier: ExecutionTier,
    },
    ValidateFailed {
        tier: ExecutionTier,
        brief: String,
        failure_hash: String,
        compile_ok: bool,
        run_ok: bool,
        log_excerpts: Option<String>,
    },
    BatchAuthoringFailed {
        tier: ExecutionTier,
        kind: FailureKind,
        brief: String,
    },
    BatchAuthoringRecovered,

    RepairSucceeded,
    RepairExhausted,

    MutationRecorded {
        op: MutationOp,
        paths: Vec<String>,
        select_terms: Vec<String>,
    },
    ProbeAttemptRecorded {
        sql: String,
        ok: bool,
        signature: ProbeSignature,
    },

    PatchImplIntentSet {
        phase: Phase,
    },
    PatchImplIntentCleared,

    PlanRevisionRequested {
        violations: Vec<PlanViolation>,
        strategy: PlanRevisionStrategy,
    },

    PublishPlanPending {
        sha256: String,
    },
    PublishCompleted {
        sha256: String,
    },
    PublishApprovalSet {
        decision: PublishApprovalDecision,
    },
    PublishApprovalCleared,
    PublishRetryBumped {
        kind: PublishRetryKind,
        cap: usize,
    },
    PublishRetriesReset,

    MarkedFailed {
        reason: String,
    },

    PlanBootstrapDone {
        phase: Phase,
    },
    PlanBootstrapReset {
        phase: Phase,
    },

    SubjectiveRetryBumped {
        kind: SubjectiveRetryKind,
        cap: usize,
    },
    SubjectiveRetriesCleared {
        kinds: Vec<SubjectiveRetryKind>,
    },

    ManifestLookupRecorded {
        path_kind: ManifestLookupPathKind,
        success: bool,
        failure_kind: Option<ManifestLookupFailureKind>,
    },

    PlanRevisionConsumed,
    InfraTransientCleared,

    ValidateCheckFailed {
        failure_context: ValidationFailureContext,
    },
}

impl ExecutionState {
    pub fn repair_context(&self) -> RepairContext {
        RepairContext {
            failure: self.repair.failure_context.clone(),
            status: self.repair.status.clone(),
            recent_failed_file_ops: Vec::new(),
        }
    }

    pub fn last_validate_failed(&self) -> bool {
        self.repair.failure_context.is_some()
    }

    pub fn hard_mutation_repair_mode(&self) -> bool {
        self.repair_state().hard_mutation_repair_mode()
    }

    pub fn new() -> Self {
        Self {
            schema_version: EXECUTION_STATE_SCHEMA_VERSION,
            ..Default::default()
        }
    }

    pub fn apply_validate_success(&mut self) {
        self.phase.pending_plan_revision = None;

        self.repair.status = RepairStatus::Idle;
        self.repair.failure_context = None;
        self.repair.last_failure_hash = None;
        self.repair.mutated_since_fail = false;
        self.repair.pending_patch_impl = None;
        self.repair.infra_transient = false;

        self.publish = PublishStatus::NotRequested;

        self.telemetry.probe = ProbeStatus::NotRequired;

        self.clear_subjective_retry_kind(SubjectiveRetryKind::ValidatePrecheckFailed);
        self.clear_subjective_retry_kind(SubjectiveRetryKind::ValidateExecutionFailed);
        self.clear_subjective_retry_kind(SubjectiveRetryKind::ValidateFailedRetry);
        self.debug_assert_invariants();
    }

    pub fn apply_repair_succeeded(&mut self) {
        self.repair.status = RepairStatus::Idle;
        self.repair.failure_context = None;
        self.repair.last_failure_hash = None;
        self.repair.mutated_since_fail = false;
        self.repair.pending_patch_impl = None;
        self.repair.infra_transient = false;
        self.telemetry.probe = ProbeStatus::NotRequired;
        self.debug_assert_invariants();
    }

    pub fn phase_state(&self) -> &PhaseState {
        &self.phase
    }

    pub(crate) fn with_phase_state_mut(&mut self, mutate: impl FnOnce(&mut PhaseState)) {
        mutate(&mut self.phase);
        self.debug_assert_invariants();
    }

    pub fn repair_state(&self) -> &RepairState {
        &self.repair
    }

    fn with_repair_state_mut(&mut self, mutate: impl FnOnce(&mut RepairState)) {
        mutate(&mut self.repair);
        self.debug_assert_invariants();
    }

    pub fn publish_status(&self) -> &PublishStatus {
        &self.publish
    }

    pub fn probe_status(&self) -> &ProbeStatus {
        &self.telemetry.probe
    }

    pub fn manifest_state(&self) -> &ManifestState {
        &self.manifest
    }

    pub fn telemetry(&self) -> &TelemetryState {
        &self.telemetry
    }

    pub fn subjective_retries(&self) -> &BTreeMap<SubjectiveRetryKind, usize> {
        &self.subjective_retries
    }

    fn with_manifest_state_mut(&mut self, mutate: impl FnOnce(&mut ManifestState)) {
        mutate(&mut self.manifest);
        self.debug_assert_invariants();
    }

    pub fn reset_manifest_lookup_state(&mut self) {
        self.with_manifest_state_mut(|manifest| {
            manifest.manifest_lookup = ManifestLookupState::default();
        });
    }

    pub fn needs_plan_bootstrap(&self, phase: Phase) -> bool {
        match phase {
            Phase::CleansePlan => !self.manifest.cleanse_plan_bootstrapped,
            Phase::ModelPlan => !self.manifest.model_plan_bootstrapped,
            _ => false,
        }
    }

    pub fn mark_plan_bootstrap_done(&mut self, phase: Phase) {
        self.with_manifest_state_mut(|manifest| match phase {
            Phase::CleansePlan => manifest.cleanse_plan_bootstrapped = true,
            Phase::ModelPlan => manifest.model_plan_bootstrapped = true,
            _ => {}
        });
    }

    pub fn reset_plan_bootstrap(&mut self, phase: Phase) {
        self.with_manifest_state_mut(|manifest| match phase {
            Phase::CleansePlan => manifest.cleanse_plan_bootstrapped = false,
            Phase::ModelPlan => manifest.model_plan_bootstrapped = false,
            _ => {}
        });
    }

    pub fn note_manifest_lookup_attempt(
        &mut self,
        path_kind: ManifestLookupPathKind,
        success: bool,
        failure_kind: Option<ManifestLookupFailureKind>,
    ) {
        let mut manifest = self.manifest.clone();
        if matches!(
            path_kind,
            ManifestLookupPathKind::Ambiguous | ManifestLookupPathKind::NonCanonical
        ) {
            manifest.manifest_lookup.noncanonical_attempt_count = manifest
                .manifest_lookup
                .noncanonical_attempt_count
                .saturating_add(1);
        }
        if success {
            if path_kind == ManifestLookupPathKind::CanonicalTarget {
                manifest.manifest_lookup.canonical_success_count = manifest
                    .manifest_lookup
                    .canonical_success_count
                    .saturating_add(1);
            }
            manifest.manifest_lookup.retry_suppressed =
                manifest.manifest_lookup.repeated_failure_count >= 2
                    && manifest.manifest_lookup.canonical_success_count == 0;
            self.manifest = manifest;
            self.debug_assert_invariants();
            return;
        }
        if let Some(kind) = failure_kind {
            let signature = format!("{}:{path_kind:?}", kind.as_str());
            let repeated = if manifest
                .manifest_lookup
                .failure_signature
                .as_deref()
                .map(|s| s == signature.as_str())
                .unwrap_or(false)
            {
                manifest
                    .manifest_lookup
                    .repeated_failure_count
                    .saturating_add(1)
            } else {
                1
            };
            manifest.manifest_lookup.failure_signature = Some(signature);
            manifest.manifest_lookup.repeated_failure_count = repeated;
        }
        manifest.manifest_lookup.retry_suppressed = manifest.manifest_lookup.repeated_failure_count
            >= 2
            && manifest.manifest_lookup.canonical_success_count == 0;
        self.manifest = manifest;
        self.debug_assert_invariants();
    }

    pub fn apply_validate_failure(
        &mut self,
        brief: String,
        failure_hash: String,
        compile_ok: bool,
        run_ok: bool,
        log_excerpts: Option<String>,
    ) {
        let repeated_failure =
            self.repair.last_failure_hash.as_deref() == Some(failure_hash.as_str());
        let next_cycle = if repeated_failure {
            self.repair.cycle_count().saturating_add(1)
        } else {
            self.clear_subjective_retry_kind(SubjectiveRetryKind::ValidateFailedRetry);
            1
        };
        self.repair.failure_context = Some(ValidationFailureContext {
            brief: brief.clone(),
            log_excerpts,
            compile_ok,
            run_ok,
        });
        self.repair.last_failure_hash = Some(failure_hash);
        self.repair.mutated_since_fail = false;
        self.repair.status = RepairStatus::Pending { cycle: next_cycle };
        self.publish = PublishStatus::NotRequested;
        self.telemetry.probe = if compile_ok {
            ProbeStatus::Required {
                attempts: ProbeAttempts::default(),
            }
        } else {
            ProbeStatus::NotRequired
        };
        self.debug_assert_invariants();
    }

    pub fn note_probe_attempt(
        &mut self,
        sql: &str,
        ok: bool,
        signature: ProbeSignature,
    ) -> ProbeOutcomeKind {
        let meaningful_sql = is_meaningful_probe_sql(sql);
        let attempts = match &mut self.telemetry.probe {
            ProbeStatus::Required { attempts } => attempts,
            ProbeStatus::Satisfied { attempts } => attempts,
            ProbeStatus::ExhaustedRequireMutation { attempts } => attempts,
            ProbeStatus::NotRequired => {
                return ProbeOutcomeKind::Failed;
            }
        };
        attempts.total = attempts.total.saturating_add(1);
        let outcome = if !ok {
            attempts.failed = attempts.failed.saturating_add(1);
            attempts.repeated_signature_streak =
                attempts.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::Failed
        } else if !meaningful_sql {
            attempts.non_meaningful = attempts.non_meaningful.saturating_add(1);
            attempts.repeated_signature_streak =
                attempts.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::NonMeaningful
        } else if attempts.last_signature.as_ref() == Some(&signature) {
            attempts.meaningful = attempts.meaningful.saturating_add(1);
            attempts.repeated_signature_streak =
                attempts.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::MeaningfulSameSignal
        } else {
            attempts.meaningful = attempts.meaningful.saturating_add(1);
            attempts.repeated_signature_streak = 0;
            ProbeOutcomeKind::MeaningfulNewSignal
        };
        attempts.last_signature = Some(signature.clone());

        if attempts.repeated_signature_streak >= 3
            || attempts.non_meaningful >= 3
            || attempts.failed >= 3
        {
            let a = attempts.clone();
            self.telemetry.probe = ProbeStatus::ExhaustedRequireMutation { attempts: a };
        } else if attempts.meaningful > 0 {
            let a = attempts.clone();
            self.telemetry.probe = ProbeStatus::Satisfied { attempts: a };
        }

        self.debug_assert_invariants();
        outcome
    }

    pub fn probe_requirement_status(&self) -> ProbeRequirementStatus {
        if !self.last_validate_failed() {
            return ProbeRequirementStatus::NotRequired;
        }
        match &self.telemetry.probe {
            ProbeStatus::NotRequired => ProbeRequirementStatus::NotRequired,
            ProbeStatus::Required { attempts } => {
                if attempts.meaningful == 0 {
                    ProbeRequirementStatus::Required
                } else {
                    ProbeRequirementStatus::Allowed
                }
            }
            ProbeStatus::Satisfied { .. } => ProbeRequirementStatus::Allowed,
            ProbeStatus::ExhaustedRequireMutation { .. } => {
                ProbeRequirementStatus::ExhaustedRequireMutation
            }
        }
    }

    pub fn bump_subjective_retry(&mut self, kind: SubjectiveRetryKind, cap: usize) -> usize {
        let entry = self.subjective_retries.entry(kind).or_insert(0);
        *entry = (*entry).saturating_add(1).min(cap.max(1));
        *entry
    }

    pub fn clear_subjective_retry_kind(&mut self, kind: SubjectiveRetryKind) {
        self.subjective_retries.remove(&kind);
    }

    pub fn clear_subjective_retries_matching(&mut self, f: impl Fn(&SubjectiveRetryKind) -> bool) {
        self.subjective_retries.retain(|k, _| !f(k));
    }

    pub fn set_pending_patch_impl_intent(&mut self, phase: Phase) {
        self.with_repair_state_mut(|repair| {
            repair.pending_patch_impl = Some(PatchImplIntent {
                phase,
                mutated_since_set: false,
            });
        });
    }

    pub fn set_pending_plan_revision(
        &mut self,
        violations: Vec<PlanViolation>,
        strategy: PlanRevisionStrategy,
    ) {
        self.with_phase_state_mut(|phase| {
            phase.pending_plan_revision = Some(PlanRevisionIntent {
                violations,
                strategy,
            });
        });
    }

    pub fn clear_pending_patch_impl(&mut self) {
        self.with_repair_state_mut(|repair| {
            repair.pending_patch_impl = None;
        });
    }

    pub fn set_publish_approval(&mut self, decision: PublishApprovalDecision) {
        match decision {
            PublishApprovalDecision::Approved => {
                let sha = self.publish.plan_sha256().map(|s| s.to_string());
                self.publish = PublishStatus::Approved { plan_sha256: sha };
            }
            PublishApprovalDecision::AwaitingUserApproval => {
                let sha = self.publish.plan_sha256().map(|s| s.to_string());
                self.publish = PublishStatus::AwaitingApproval {
                    plan_sha256: sha,
                    approval_retries: 0,
                };
            }
            PublishApprovalDecision::Rejected => {
                self.publish = PublishStatus::NotRequested;
            }
        }
        self.debug_assert_invariants();
    }

    pub fn clear_publish_approval(&mut self) {
        self.publish = PublishStatus::NotRequested;
        self.debug_assert_invariants();
    }

    pub fn is_publish_approved(&self) -> bool {
        self.publish.is_approved()
    }

    pub fn bump_publish_retry(&mut self, kind: PublishRetryKind, cap: usize) -> usize {
        let capped = cap.max(1);
        match kind {
            PublishRetryKind::AwaitApprovalLoop => {
                if let PublishStatus::AwaitingApproval {
                    approval_retries, ..
                } = &mut self.publish
                {
                    *approval_retries = (*approval_retries).saturating_add(1).min(capped);
                    let count = *approval_retries;
                    self.debug_assert_invariants();
                    return count;
                }
                let sha = self.publish.plan_sha256().map(|s| s.to_string());
                self.publish = PublishStatus::AwaitingApproval {
                    plan_sha256: sha,
                    approval_retries: 1,
                };
                self.debug_assert_invariants();
                1
            }
            PublishRetryKind::PublishFailureLoop => {
                if let PublishStatus::Failed { publish_retries } = &mut self.publish {
                    *publish_retries = (*publish_retries).saturating_add(1).min(capped);
                    let count = *publish_retries;
                    self.debug_assert_invariants();
                    return count;
                }
                self.publish = PublishStatus::Failed { publish_retries: 1 };
                self.debug_assert_invariants();
                1
            }
        }
    }

    pub fn reset_publish_retry(&mut self, kind: PublishRetryKind) {
        match kind {
            PublishRetryKind::AwaitApprovalLoop => {
                if let PublishStatus::AwaitingApproval {
                    approval_retries, ..
                } = &mut self.publish
                {
                    *approval_retries = 0;
                }
            }
            PublishRetryKind::PublishFailureLoop => {
                if let PublishStatus::Failed { publish_retries } = &mut self.publish {
                    *publish_retries = 0;
                }
            }
        }
        self.debug_assert_invariants();
    }

    pub fn reset_publish_retries(&mut self) {
        self.publish = PublishStatus::NotRequested;
        self.debug_assert_invariants();
    }

    pub fn mark_failed(&mut self, brief: impl Into<String>) {
        let brief_str = brief.into();
        self.repair.failure_context = Some(ValidationFailureContext {
            brief: brief_str,
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });
        self.debug_assert_invariants();
    }

    pub async fn load(
        control: &ControlStateStore,
        thread_id: &str,
    ) -> Result<Option<Self>, String> {
        crate::state_manager::load_execution_state(control, thread_id)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn load_strict(
        control: &ControlStateStore,
        thread_id: &str,
    ) -> Result<Option<Self>, String> {
        crate::state_manager::load_execution_state_strict(control, thread_id)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn save(&self, control: &ControlStateStore, thread_id: &str) -> Result<(), String> {
        crate::state_manager::replace_execution_state(control, thread_id, self.clone())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn set_pending_publish_plan(&mut self, plan_sha256: String) {
        self.publish = PublishStatus::PlanPending { plan_sha256 };
        self.debug_assert_invariants();
    }

    pub fn mark_publish_complete(&mut self, plan_sha256: String) {
        self.publish = PublishStatus::Succeeded { plan_sha256 };
        self.debug_assert_invariants();
    }

    pub fn set_artifact_focus(
        &mut self,
        kind: Option<String>,
        name: Option<String>,
        dataset_id: Option<String>,
        exists: bool,
    ) {
        self.telemetry.artifact_focus = Some(ArtifactFocusState {
            kind,
            name,
            dataset_id,
            exists,
            ts: Some(chrono::Utc::now().to_rfc3339()),
        });
    }

    pub fn set_last_mutation_summary(
        &mut self,
        op: MutationOp,
        affected_paths: Vec<String>,
        select_terms: Vec<String>,
    ) {
        self.repair.mutated_since_fail = true;
        if matches!(
            self.telemetry.probe,
            ProbeStatus::ExhaustedRequireMutation { .. }
        ) {
            self.telemetry.probe = ProbeStatus::NotRequired;
        }
        if let Some(ref mut intent) = self.repair.pending_patch_impl {
            intent.mutated_since_set = true;
        }
        self.telemetry.last_mutation_summary = Some(LastMutationSummary {
            op,
            affected_paths,
            select_terms,
            ts: Some(chrono::Utc::now().to_rfc3339()),
        });
        self.debug_assert_invariants();
    }

    pub fn apply_event(&mut self, event: DataEngineerEvent) {
        match event {
            DataEngineerEvent::ValidatePassed { tier: _ } => self.apply_validate_success(),
            DataEngineerEvent::ValidateFailed {
                tier: _,
                brief,
                failure_hash,
                compile_ok,
                run_ok,
                log_excerpts,
            } => {
                self.apply_validate_failure(brief, failure_hash, compile_ok, run_ok, log_excerpts);
            }
            DataEngineerEvent::BatchAuthoringFailed {
                tier: _,
                kind,
                brief,
            } => {
                if kind == FailureKind::InfraTransient {
                    self.repair.infra_transient = true;
                    self.repair.failure_context = Some(ValidationFailureContext {
                        brief,
                        log_excerpts: None,
                        compile_ok: false,
                        run_ok: false,
                    });
                    self.debug_assert_invariants();
                    return;
                }
                let hash = sha256_hex(brief.trim());
                self.apply_validate_failure(brief, hash, false, false, None);
            }
            DataEngineerEvent::BatchAuthoringRecovered => {
                self.repair.status = RepairStatus::Idle;
                self.repair.failure_context = None;
                self.repair.pending_patch_impl = None;
                self.telemetry.probe = ProbeStatus::NotRequired;
            }
            DataEngineerEvent::RepairSucceeded => {
                self.apply_repair_succeeded();
            }
            DataEngineerEvent::RepairExhausted => {
                let cycles = self.repair.cycle_count();
                self.repair.status = RepairStatus::Exhausted {
                    cycles_used: cycles,
                };
            }
            DataEngineerEvent::MutationRecorded {
                op,
                paths,
                select_terms,
            } => {
                self.set_last_mutation_summary(op, paths, select_terms);
            }
            DataEngineerEvent::ProbeAttemptRecorded { sql, ok, signature } => {
                let _ = self.note_probe_attempt(&sql, ok, signature);
            }
            DataEngineerEvent::PatchImplIntentSet { phase } => {
                self.set_pending_patch_impl_intent(phase);
            }
            DataEngineerEvent::PatchImplIntentCleared => {
                self.clear_pending_patch_impl();
            }
            DataEngineerEvent::PlanRevisionRequested {
                violations,
                strategy,
            } => {
                self.set_pending_plan_revision(violations, strategy);
            }
            DataEngineerEvent::PublishPlanPending { sha256 } => {
                self.set_pending_publish_plan(sha256);
            }
            DataEngineerEvent::PublishCompleted { sha256 } => {
                self.mark_publish_complete(sha256);
            }
            DataEngineerEvent::PublishApprovalSet { decision } => {
                self.set_publish_approval(decision);
            }
            DataEngineerEvent::PublishApprovalCleared => {
                self.clear_publish_approval();
            }
            DataEngineerEvent::PublishRetryBumped { kind, cap } => {
                self.bump_publish_retry(kind, cap);
                return; // bump_publish_retry already calls debug_assert_invariants
            }
            DataEngineerEvent::PublishRetriesReset => {
                self.reset_publish_retries();
            }
            DataEngineerEvent::MarkedFailed { reason } => {
                self.mark_failed(reason);
            }
            DataEngineerEvent::PlanBootstrapDone { phase } => {
                self.mark_plan_bootstrap_done(phase);
            }
            DataEngineerEvent::PlanBootstrapReset { phase } => {
                self.reset_plan_bootstrap(phase);
            }
            DataEngineerEvent::SubjectiveRetryBumped { kind, cap } => {
                self.bump_subjective_retry(kind, cap);
            }
            DataEngineerEvent::SubjectiveRetriesCleared { kinds } => {
                for kind in kinds {
                    self.clear_subjective_retry_kind(kind);
                }
            }
            DataEngineerEvent::ManifestLookupRecorded {
                path_kind,
                success,
                failure_kind,
            } => {
                self.note_manifest_lookup_attempt(path_kind, success, failure_kind);
            }
            DataEngineerEvent::PlanRevisionConsumed => {
                self.phase.pending_plan_revision = None;
            }
            DataEngineerEvent::InfraTransientCleared => {
                self.repair.infra_transient = false;
            }
            DataEngineerEvent::ValidateCheckFailed { failure_context } => {
                let next_cycle = self.repair.cycle_count().saturating_add(1);
                self.repair.failure_context = Some(failure_context);
                self.repair.last_failure_hash = None;
                self.repair.mutated_since_fail = false;
                self.repair.status = RepairStatus::Pending { cycle: next_cycle };
            }
        }
        self.debug_assert_invariants();
    }

    pub fn validate_invariants(&self) -> Result<(), String> {
        let mut violations = Vec::new();
        self.collect_phase_coherence_violations(&mut violations);
        self.collect_probe_lifecycle_violations(&mut violations);
        if violations.is_empty() {
            return Ok(());
        }
        Err(format!(
            "execution_state invariant violation(s): {}",
            violations.join("; ")
        ))
    }

    fn collect_phase_coherence_violations(&self, violations: &mut Vec<String>) {
        let phase = self.phase_state();
        if phase.transition.is_some() && phase.current_phase == Phase::Preflight {
            violations.push("transition set while current_phase is Preflight".to_string());
        }
    }

    fn collect_probe_lifecycle_violations(&self, violations: &mut Vec<String>) {
        let is_active_probe = !matches!(self.telemetry.probe, ProbeStatus::NotRequired);
        if is_active_probe && !self.last_validate_failed() {
            violations.push(
                "probe status can only be active while last_validate_failed is true".to_string(),
            );
        }
    }

    fn debug_assert_invariants(&self) {
        debug_assert!(
            self.validate_invariants().is_ok(),
            "invalid execution state: {:?}",
            self.validate_invariants()
        );
    }
}

pub fn normalize_manifest_path(path: &str) -> String {
    path.trim().trim_matches('/').replace('\\', "/")
}

pub fn classify_manifest_lookup_path(path: &str) -> Option<ManifestLookupPathKind> {
    let norm = normalize_manifest_path(path);
    if norm.is_empty() {
        return None;
    }
    if norm == "target/manifest.json" {
        return Some(ManifestLookupPathKind::CanonicalTarget);
    }
    if norm.ends_with("target/manifest.json") {
        return Some(ManifestLookupPathKind::NonCanonical);
    }
    if norm.ends_with("manifest.json") {
        return Some(ManifestLookupPathKind::Ambiguous);
    }
    None
}

pub fn classify_manifest_lookup_failure(errors: &[String]) -> Option<ManifestLookupFailureKind> {
    let joined = errors.join("\n").to_ascii_lowercase();
    if joined.contains("nosuchkey")
        || joined.contains("not found or failed to fetch")
        || joined.contains("the specified key does not exist")
    {
        return Some(ManifestLookupFailureKind::NoSuchKey);
    }
    if joined.contains("pointer not found") {
        return Some(ManifestLookupFailureKind::PointerNotFound);
    }
    None
}

pub fn sha256_hex(input: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn is_sql_model_path(path: &str) -> bool {
    let normalized = path.trim().replace('\\', "/");
    normalized.starts_with("models/")
        && normalized.ends_with(".sql")
        && !normalized.contains(".yml/")
        && !normalized.contains(".yaml/")
}

fn is_schema_doc_path(path: &str) -> bool {
    let normalized = path.trim().replace('\\', "/");
    normalized.starts_with("models/")
        && (normalized.ends_with(".yml") || normalized.ends_with(".yaml"))
}

pub fn gate_authoring_probe(state: &ExecutionState) -> Result<(), String> {
    match state.probe_requirement_status() {
        ProbeRequirementStatus::Required => Err(
            "progress_gate_blocked: runtime validation previously failed after compile and a meaningful data probe is still required"
                .to_string(),
        ),
        ProbeRequirementStatus::ExhaustedRequireMutation => Err(
            "progress_gate_blocked: probe loop exhausted (repeated/no-new-signal probes); apply a mutating fix before validating"
                .to_string(),
        ),
        ProbeRequirementStatus::NotRequired | ProbeRequirementStatus::Allowed => Ok(()),
    }
}

pub fn is_meaningful_probe_sql(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').trim().to_lowercase();
    if s.is_empty() {
        return false;
    }
    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks == ["select", "1"] {
        return false;
    }
    if toks.len() == 4 && toks[0] == "select" && toks[1] == "1" && toks[2] == "as" {
        return false;
    }
    toks.iter().any(|t| *t == "from")
}

pub fn gate_publish_progress(state: &ExecutionState, phase: Phase) -> Result<(), String> {
    match phase {
        Phase::PublishAwaitApproval | Phase::Publish => {}
        _ => return Ok(()),
    }
    if state.is_publish_approved() {
        return Ok(());
    }
    Err("publish_gate_blocked: publish requires explicit persisted approval state".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_core::session::ControlStateStore;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[test]
    fn state_has_compile_time_defaults() {
        let st = ExecutionState::new();
        assert_eq!(st.phase.current_phase, Phase::Preflight);
    }

    #[test]
    fn repair_context_includes_recent_failed_file_ops() {
        let ctx = RepairContext {
            failure: None,
            status: RepairStatus::Idle,
            recent_failed_file_ops: vec![RecentFailedFileOp {
                op: "patch".to_string(),
                path: "models/staging/stg_orders.yml".to_string(),
                error_brief: "invalid YAML: duplicate entry with key \"version\"".to_string(),
                count: 1,
            }],
        };
        let rendered = ctx.format_error_context();
        assert!(ctx.has_context());
        assert!(rendered.contains("Recent failed file mutations"));
        assert!(rendered.contains("stg_orders.yml"));
        assert!(rendered.contains("duplicate entry with key"));
    }

    #[test]
    fn validate_success_resets_repair_and_retry_state() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Pending { cycle: 1 };
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "some error".to_string(),
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });
        st.subjective_retries
            .insert(SubjectiveRetryKind::PlanSemanticInvalid, 3);
        st.subjective_retries
            .insert(SubjectiveRetryKind::ValidatePrecheckFailed, 2);
        st.apply_validate_success();
        assert_eq!(st.phase.current_phase.tier(), ExecutionTier::Unknown);
        assert!(!st.hard_mutation_repair_mode());
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::PlanSemanticInvalid),
            Some(&3)
        );
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::ValidatePrecheckFailed),
            None
        );
    }

    #[test]
    fn repair_succeeded_preserves_subjective_retry_counters() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Pending { cycle: 2 };
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "precheck error".to_string(),
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });
        st.subjective_retries
            .insert(SubjectiveRetryKind::ValidatePrecheckFailed, 2);
        st.subjective_retries
            .insert(SubjectiveRetryKind::ValidateExecutionFailed, 1);

        st.apply_repair_succeeded();

        assert_eq!(st.repair.status, RepairStatus::Idle);
        assert!(st.repair.failure_context.is_none());
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::ValidatePrecheckFailed),
            Some(&2),
            "precheck retry counter must survive repair_succeeded"
        );
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::ValidateExecutionFailed),
            Some(&1),
            "execution-failed retry counter must survive repair_succeeded"
        );
    }

    #[test]
    fn validate_failure_activates_repair() {
        let mut st = ExecutionState::new();
        st.apply_validate_failure(
            "schema fail".to_string(),
            sha256_hex("schema fail"),
            true,
            false,
            None,
        );
        assert!(st.hard_mutation_repair_mode());
        assert!(matches!(
            st.repair.status,
            RepairStatus::Pending { cycle: 1 }
        ));
        assert_eq!(
            st.repair.failure_context.as_ref().unwrap().brief,
            "schema fail"
        );
        assert_eq!(st.repair.cycle_count(), 1);
    }

    #[test]
    fn sql_model_path_rejects_dbt_compiled_test_paths() {
        assert!(SqlModelPath::parse("models/staging/stg_orders.sql").is_ok());
        assert!(SqlModelPath::parse(
            "models/staging/stg_orders.yml/not_null_stg_orders_order_id.sql"
        )
        .is_err());
        assert!(SqlModelPath::parse(
            "models/staging/stg_orders.yaml/not_null_stg_orders_order_id.sql"
        )
        .is_err());
        let target = RepairTargetPath::parse(
            "models/staging/stg_orders.yml/not_null_stg_orders_order_id.sql".to_string(),
        );
        assert!(
            target.is_err(),
            "dbt compiled test path must not parse as RepairTargetPath"
        );
    }

    #[test]
    fn subjective_retry_per_kind_and_bounded() {
        let mut st = ExecutionState::new();
        assert_eq!(
            st.bump_subjective_retry(SubjectiveRetryKind::PlanSemanticInvalid, 3),
            1
        );
        assert_eq!(
            st.bump_subjective_retry(SubjectiveRetryKind::PlanSemanticInvalid, 3),
            2
        );
        assert_eq!(
            st.bump_subjective_retry(SubjectiveRetryKind::PlanSemanticInvalid, 3),
            3
        );
        assert_eq!(
            st.bump_subjective_retry(SubjectiveRetryKind::PlanSemanticInvalid, 3),
            3
        );

        assert_eq!(
            st.bump_subjective_retry(SubjectiveRetryKind::PlanGroundingEmptyAfterPrune, 3),
            1
        );
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::PlanSemanticInvalid),
            Some(&3)
        );

        st.clear_subjective_retry_kind(SubjectiveRetryKind::PlanSemanticInvalid);
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::PlanSemanticInvalid),
            None
        );
        assert_eq!(
            st.subjective_retries
                .get(&SubjectiveRetryKind::PlanGroundingEmptyAfterPrune),
            Some(&1)
        );

        st.clear_subjective_retries_matching(|k| {
            matches!(k, SubjectiveRetryKind::PlanGroundingEmptyAfterPrune)
        });
        assert!(st.subjective_retries.is_empty());
    }

    #[test]
    fn gate_authoring_probe_rejects_when_probe_required() {
        let mut st = ExecutionState::new();
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "test".to_string(),
            log_excerpts: None,
            compile_ok: true,
            run_ok: false,
        });
        st.telemetry.probe = ProbeStatus::Required {
            attempts: ProbeAttempts::default(),
        };
        assert!(gate_authoring_probe(&st).is_err());
    }

    #[test]
    fn gate_authoring_probe_accepts_when_no_probe_required() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Pending { cycle: 1 };
        st.telemetry.probe = ProbeStatus::NotRequired;
        assert!(gate_authoring_probe(&st).is_ok());
    }

    #[test]
    fn probe_status_allows_multiple_meaningful_probes_and_exhausts_on_repeats() {
        let mut st = ExecutionState::new();
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "test".to_string(),
            log_excerpts: None,
            compile_ok: true,
            run_ok: false,
        });
        st.telemetry.probe = ProbeStatus::Required {
            attempts: ProbeAttempts::default(),
        };

        let sig1 = ProbeSignature::from_run_sql(
            "select * from x limit 10",
            &serde_json::json!({"ok":true}),
        );
        let out1 = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        assert_eq!(out1, ProbeOutcomeKind::MeaningfulNewSignal);
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::Allowed
        );

        let sig2 = ProbeSignature::from_run_sql(
            "select * from y limit 10",
            &serde_json::json!({"ok":true}),
        );
        let out2 = st.note_probe_attempt("select * from y limit 10", true, sig2);
        assert_eq!(out2, ProbeOutcomeKind::MeaningfulNewSignal);
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::Allowed
        );

        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1);
        let _ = st.note_probe_attempt(
            "select * from x limit 10",
            true,
            ProbeSignature::from_run_sql(
                "select * from x limit 10",
                &serde_json::json!({"ok":true}),
            ),
        );
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::ExhaustedRequireMutation
        );
    }

    #[test]
    fn successful_mutation_unblocks_exhausted_probe_cycle() {
        let mut st = ExecutionState::new();
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "test".to_string(),
            log_excerpts: None,
            compile_ok: true,
            run_ok: false,
        });
        st.telemetry.probe = ProbeStatus::ExhaustedRequireMutation {
            attempts: ProbeAttempts::default(),
        };
        st.apply_event(DataEngineerEvent::MutationRecorded {
            op: MutationOp::Patch,
            paths: vec!["models/staging/stg_orders.sql".to_string()],
            select_terms: Vec::new(),
        });
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::NotRequired
        );
        assert!(st.repair.mutated_since_fail);
    }

    #[test]
    fn mark_failed_sets_failure_context() {
        let mut st = ExecutionState::new();
        st.phase.current_phase = Phase::CleanseValidate;
        st.mark_failed("x");
        assert_eq!(st.repair.failure_context.as_ref().unwrap().brief, "x");
    }

    #[test]
    fn publish_gate_requires_explicit_approval() {
        let mut st = ExecutionState::new();
        assert!(gate_publish_progress(&st, Phase::Publish).is_err());
        st.set_publish_approval(PublishApprovalDecision::Approved);
        assert!(gate_publish_progress(&st, Phase::PublishAwaitApproval).is_ok());
        assert!(gate_publish_progress(&st, Phase::Publish).is_ok());
        st.set_publish_approval(PublishApprovalDecision::Rejected);
        assert!(gate_publish_progress(&st, Phase::Publish).is_err());
    }

    #[test]
    fn publish_retry_budget_is_tracked_by_typed_kind() {
        let mut st = ExecutionState::new();
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::AwaitApprovalLoop, 3),
            1
        );
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::AwaitApprovalLoop, 3),
            2
        );
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::PublishFailureLoop, 3),
            1
        );
        st.reset_publish_retry(PublishRetryKind::AwaitApprovalLoop);
        if let PublishStatus::AwaitingApproval {
            approval_retries, ..
        } = &st.publish
        {
            assert_eq!(
                *approval_retries, 0,
                "approval retries should be reset to 0"
            );
        }
    }

    #[test]
    fn validate_failures_increment_repair_cycles() {
        let mut st = ExecutionState::new();

        st.apply_validate_failure(
            "error A".to_string(),
            sha256_hex("error A"),
            true,
            false,
            None,
        );
        assert_eq!(st.repair.cycle_count(), 1);

        st.apply_validate_failure(
            "error A".to_string(),
            sha256_hex("error A"),
            true,
            false,
            None,
        );
        assert_eq!(st.repair.cycle_count(), 2);

        st.apply_validate_failure(
            "error B".to_string(),
            sha256_hex("error B"),
            true,
            false,
            None,
        );
        assert_eq!(
            st.repair.cycle_count(),
            1,
            "a new failure signature should reset the repair-cycle stall counter"
        );
    }

    #[test]
    fn new_validate_failure_signature_clears_validate_retry_budget() {
        let mut st = ExecutionState::new();

        st.apply_validate_failure(
            "error A".to_string(),
            sha256_hex("error A"),
            true,
            false,
            None,
        );
        st.bump_subjective_retry(SubjectiveRetryKind::ValidateFailedRetry, 4);
        st.bump_subjective_retry(SubjectiveRetryKind::ValidateFailedRetry, 4);
        assert_eq!(
            st.subjective_retries()
                .get(&SubjectiveRetryKind::ValidateFailedRetry)
                .copied(),
            Some(2)
        );

        st.apply_validate_failure(
            "error B".to_string(),
            sha256_hex("error B"),
            true,
            false,
            None,
        );
        assert!(
            st.subjective_retries()
                .get(&SubjectiveRetryKind::ValidateFailedRetry)
                .is_none(),
            "new failure signatures should reset the validate retry budget"
        );
    }

    #[test]
    fn probe_attempt_recorded_advances_probe_requirement() {
        let mut st = ExecutionState::new();
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "test".to_string(),
            log_excerpts: None,
            compile_ok: true,
            run_ok: false,
        });
        st.telemetry.probe = ProbeStatus::Required {
            attempts: ProbeAttempts::default(),
        };

        st.apply_event(DataEngineerEvent::ProbeAttemptRecorded {
            sql: "select * from x limit 10".to_string(),
            ok: true,
            signature: ProbeSignature::from_run_sql(
                "select * from x limit 10",
                &serde_json::json!({"ok":true}),
            ),
        });
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::Allowed
        );
    }

    #[test]
    fn apply_event_batch_authoring_failed_activates_repair() {
        let mut st = ExecutionState::new();
        st.apply_event(DataEngineerEvent::BatchAuthoringFailed {
            tier: ExecutionTier::Cleanse,
            kind: FailureKind::Unknown,
            brief: "sql validation failed".to_string(),
        });
        assert!(st.hard_mutation_repair_mode());
        assert!(st.last_validate_failed());
    }

    #[test]
    fn apply_event_batch_authoring_failed_infra_transient_skips_repair() {
        let mut st = ExecutionState::new();
        st.apply_event(DataEngineerEvent::BatchAuthoringFailed {
            tier: ExecutionTier::Cleanse,
            kind: FailureKind::InfraTransient,
            brief: "service error".to_string(),
        });
        assert!(
            !st.hard_mutation_repair_mode(),
            "infra-transient must not enter repair mode"
        );
        assert!(
            st.repair.infra_transient,
            "flag must be set for step-boundary short-circuit"
        );
        assert_eq!(
            st.repair.cycle_count(),
            0,
            "repair_cycles must not increment"
        );
        assert_eq!(
            st.repair.failure_context.as_ref().unwrap().brief,
            "service error"
        );
    }

    #[test]
    fn apply_event_infra_transient_cleared() {
        let mut st = ExecutionState::new();
        st.repair.infra_transient = true;
        st.apply_event(DataEngineerEvent::InfraTransientCleared);
        assert!(!st.repair.infra_transient);
    }

    #[test]
    fn apply_event_validate_check_failed_sets_failure_context() {
        let mut st = ExecutionState::new();
        assert!(st.repair.failure_context.is_none());
        assert_eq!(st.repair.status, RepairStatus::Idle);
        st.apply_event(DataEngineerEvent::ValidateCheckFailed {
            failure_context: ValidationFailureContext {
                brief: "YAML references unknown columns".to_string(),
                log_excerpts: None,
                compile_ok: false,
                run_ok: false,
            },
        });
        assert!(st.repair.failure_context.is_some());
        assert_eq!(
            st.repair.failure_context.as_ref().unwrap().brief,
            "YAML references unknown columns"
        );
        assert_eq!(st.repair.status, RepairStatus::Pending { cycle: 1 });
        assert!(!st.repair.mutated_since_fail);
    }

    #[test]
    fn apply_event_plan_revision_consumed() {
        let mut st = ExecutionState::new();
        st.phase.pending_plan_revision = Some(PlanRevisionIntent {
            violations: vec![],
            strategy: PlanRevisionStrategy::Rewrite,
        });
        st.apply_event(DataEngineerEvent::PlanRevisionConsumed);
        assert!(st.phase.pending_plan_revision.is_none());
    }

    #[test]
    fn apply_event_batch_authoring_recovered_clears_repair() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Pending { cycle: 1 };
        st.repair.failure_context = Some(ValidationFailureContext {
            brief: "some error".to_string(),
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });
        st.apply_event(DataEngineerEvent::BatchAuthoringRecovered);
        assert!(!st.hard_mutation_repair_mode());
        assert!(st.repair.failure_context.is_none());
    }

    #[test]
    fn invariants_reject_transition_on_preflight_phase() {
        let mut st = ExecutionState::new();
        st.phase.transition = Some(PhaseTransition::PreflightOk {
            dbt_project_key: "k".to_string(),
            has_query_provider: true,
            has_dbt_provider: true,
        });
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("transition set while current_phase is Preflight"));
    }

    #[test]
    fn publish_status_transitions_correctly() {
        let mut st = ExecutionState::new();
        assert!(matches!(st.publish, PublishStatus::NotRequested));
        st.set_pending_publish_plan("sha123".to_string());
        assert!(matches!(st.publish, PublishStatus::PlanPending { .. }));
        st.set_publish_approval(PublishApprovalDecision::Approved);
        assert!(st.is_publish_approved());
        st.mark_publish_complete("sha123".to_string());
        assert!(matches!(st.publish, PublishStatus::Succeeded { .. }));
    }

    #[test]
    fn invariants_reject_probe_active_when_last_validate_not_failed() {
        let mut st = ExecutionState::new();
        st.telemetry.probe = ProbeStatus::Required {
            attempts: ProbeAttempts::default(),
        };
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("probe status can only be active while last_validate_failed is true"));
    }

    #[tokio::test]
    async fn load_strict_rejects_malformed_control_state() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let control = ControlStateStore::new(storage, scope, keyspace);
        let tid = "tid-malformed-control-state";

        let malformed_envelope = serde_json::json!({
            "schema_version": react_core::session::CONTROL_STATE_ENVELOPE_SCHEMA_VERSION,
            "suite_id": "data_engineer",
            "payload": {"schema_version":"bad"}
        });
        control
            .save(tid, "data_engineer", &malformed_envelope)
            .await
            .expect("seed control state");

        let got = ExecutionState::load_strict(&control, tid).await;
        assert!(got.is_err(), "malformed control_state must fail loudly");
    }
}
