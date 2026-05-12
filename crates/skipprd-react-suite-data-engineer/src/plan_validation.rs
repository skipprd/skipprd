use crate::plan_progress::{
    checklist_status, is_runnable_checklist_status, missing_required_checklist_items,
    required_checklist_item_ids, work_groups_cover_task_checklist, CHECKLIST_SQL_MODEL,
    MAX_BATCH_SIZE,
};
use crate::plan_types::{CleansePlan, ModelPlan, Plan, PlanTask, PlanWorkGroup};

#[derive(Clone, Debug)]
pub struct PlanSemanticValidation {
    pub ok: bool,
    pub errors: Vec<String>,
    pub issues: Vec<PlanSemanticIssue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanSemanticIssueCode {
    MissingPlanKey,
    MissingTasks,
    DuplicateTaskId,
    InvalidBatch,
    MissingImplementationSpec,
    MissingChecklistItems,
    UnknownTaskReference,
    MissingWorkGroups,
    MissingWorkGroupCoverage,
    UnverifiedSemanticEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSemanticIssue {
    pub code: PlanSemanticIssueCode,
    pub task_id: Option<String>,
    pub message: String,
}

impl PlanSemanticValidation {
    pub fn messages(&self) -> Vec<String> {
        if !self.errors.is_empty() {
            return self.errors.clone();
        }
        self.issues.iter().map(|i| i.message.clone()).collect()
    }
}

fn sem(code: PlanSemanticIssueCode, message: impl Into<String>) -> PlanSemanticIssue {
    PlanSemanticIssue {
        code,
        task_id: None,
        message: message.into(),
    }
}

fn sem_task(
    code: PlanSemanticIssueCode,
    task_id: &str,
    message: impl Into<String>,
) -> PlanSemanticIssue {
    PlanSemanticIssue {
        code,
        task_id: Some(task_id.to_string()),
        message: message.into(),
    }
}

fn duplicate_values(values: &[String]) -> Vec<String> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for v in values.iter() {
        let t = v.trim();
        if t.is_empty() {
            continue;
        }
        *counts.entry(t.to_string()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

fn model_lineage_resolve_source_column(
    relation: Option<&str>,
    column: &str,
    per_input: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    task_source: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let col = column.trim();
    if col.is_empty() {
        return Err("empty column name".to_string());
    }
    let any_nonempty_schema = per_input.values().any(|s| !s.is_empty());
    if let Some(rel) = relation.map(str::trim).filter(|r| !r.is_empty()) {
        let Some(set) = per_input.get(rel) else {
            return Err(format!("unknown lineage.source.relation '{rel}'"));
        };
        if !set.is_empty() && !set.contains(col) {
            return Err(format!(
                "lineage references column '{col}' not present on relation '{rel}'"
            ));
        }
        return Ok(());
    }
    if !any_nonempty_schema {
        if task_source.contains(col) {
            return Ok(());
        }
        for set in per_input.values() {
            if set.contains(col) {
                return Ok(());
            }
        }
        return Err(format!(
            "lineage references unknown column '{col}' (no non-empty grounded schemas to disambiguate)"
        ));
    }
    let mut hits: Vec<&str> = Vec::new();
    for (inp, set) in per_input {
        if set.contains(col) {
            hits.push(inp.as_str());
        }
    }
    match hits.len() {
        0 => {
            if task_source.contains(col) {
                Ok(())
            } else {
                Err(format!(
                    "lineage references unknown column '{col}' (not found on any grounded input schema)"
                ))
            }
        }
        1 => Ok(()),
        _ => Err(format!(
            "lineage column '{col}' is ambiguous across inputs {:?}; set lineage.source.relation",
            hits
        )),
    }
}

fn validate_output_field_lineage_cleanse(
    tid: &str,
    field: &crate::plan_types::OutputFieldSpec,
    known: &std::collections::BTreeSet<String>,
    issues: &mut Vec<PlanSemanticIssue>,
) {
    use crate::plan_types::{FieldKind, LineageRole};
    use PlanSemanticIssueCode::MissingImplementationSpec;

    if field.lineage.is_empty() && field.kind == FieldKind::Raw {
        let nonempty = field
            .source_columns
            .iter()
            .filter(|s| !s.trim().is_empty())
            .count();
        if nonempty > 1 {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] kind=raw lists multiple source_columns; declare explicit lineage with exactly one passthrough mapping",
                    tid, field.name
                ),
            ));
            return;
        }
    }

    let effective = field.effective_lineage();
    if effective.is_empty() {
        issues.push(sem_task(
            MissingImplementationSpec,
            tid,
            format!(
                "{}: output_fields[{}] is missing lineage (populate lineage[] or source_columns for legacy migration)",
                tid, field.name
            ),
        ));
        return;
    }

    for ln in &effective {
        if let Some(rel) = ln.source.relation.as_deref() {
            if !rel.trim().is_empty() {
                issues.push(sem_task(
                    MissingImplementationSpec,
                    tid,
                    format!(
                        "{}: output_fields[{}] lineage.source.relation must be omitted for cleanse tasks (got '{}')",
                        tid, field.name, rel
                    ),
                ));
            }
        }
        if !known.is_empty() && !known.contains(&ln.source.name) {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] lineage references unknown source column '{}' (available: {})",
                    tid,
                    field.name,
                    ln.source.name,
                    known.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }

    if field.kind == FieldKind::Raw {
        if effective.len() != 1 || effective[0].role != LineageRole::Passthrough {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] kind=raw requires exactly one lineage entry with role=passthrough",
                    tid, field.name
                ),
            ));
        }
    }

    if !known.is_empty() && field.name.contains('.') && known.contains(field.name.as_str()) {
        if field.kind != FieldKind::Raw
            || effective.len() != 1
            || effective[0].role != LineageRole::Passthrough
            || effective[0].source.name != field.name
        {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] publishes a dotted name that exists in source_schema; require kind=raw and a single passthrough lineage from that exact field",
                    tid, field.name
                ),
            ));
        }
    }

    for ln in &effective {
        let role_ok = match field.kind {
            FieldKind::Raw => ln.role == LineageRole::Passthrough,
            FieldKind::Clean => {
                matches!(ln.role, LineageRole::Normalized | LineageRole::Parsed)
            }
            FieldKind::Derived => {
                matches!(
                    ln.role,
                    LineageRole::DerivedInput | LineageRole::Parsed | LineageRole::Normalized
                )
            }
            FieldKind::QualityFlag => matches!(ln.role, LineageRole::QualityInput),
        };
        if !role_ok {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] has kind={:?} but lineage role {:?} is inconsistent with mapping expectations",
                    tid, field.name, field.kind, ln.role
                ),
            ));
        }
    }
}

fn validate_output_field_lineage_model(
    tid: &str,
    field: &crate::plan_types::OutputFieldSpec,
    per_input: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    task_source: &std::collections::BTreeSet<String>,
    issues: &mut Vec<PlanSemanticIssue>,
) {
    use crate::plan_types::{FieldKind, LineageRole};
    use PlanSemanticIssueCode::MissingImplementationSpec;

    if field.lineage.is_empty() && field.kind == FieldKind::Raw {
        let nonempty = field
            .source_columns
            .iter()
            .filter(|s| !s.trim().is_empty())
            .count();
        if nonempty > 1 {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] kind=raw lists multiple source_columns; declare explicit lineage with exactly one passthrough mapping",
                    tid, field.name
                ),
            ));
            return;
        }
    }

    let effective = field.effective_lineage();
    if effective.is_empty() {
        issues.push(sem_task(
            MissingImplementationSpec,
            tid,
            format!(
                "{}: output_fields[{}] is missing lineage (populate lineage[] or source_columns for legacy migration)",
                tid, field.name
            ),
        ));
        return;
    }

    for ln in &effective {
        let rel = ln
            .source
            .relation
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        if let Err(msg) =
            model_lineage_resolve_source_column(rel, &ln.source.name, per_input, task_source)
        {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!("{}: output_fields[{}] {}", tid, field.name, msg),
            ));
        }
    }

    if field.kind == FieldKind::Raw {
        if effective.len() != 1 || effective[0].role != LineageRole::Passthrough {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] kind=raw requires exactly one lineage entry with role=passthrough",
                    tid, field.name
                ),
            ));
        }
    }

    let mut union_known: std::collections::BTreeSet<String> = task_source.clone();
    for set in per_input.values() {
        union_known.extend(set.iter().cloned());
    }
    if field.name.contains('.') && union_known.contains(field.name.as_str()) {
        if field.kind != FieldKind::Raw
            || effective.len() != 1
            || effective[0].role != LineageRole::Passthrough
            || effective[0].source.name != field.name
        {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] publishes a dotted name that exists on an upstream schema; require kind=raw and a single passthrough lineage from that exact field",
                    tid, field.name
                ),
            ));
        }
    }

    for ln in &effective {
        let role_ok = match field.kind {
            FieldKind::Raw => ln.role == LineageRole::Passthrough,
            FieldKind::Clean => {
                matches!(ln.role, LineageRole::Normalized | LineageRole::Parsed)
            }
            FieldKind::Derived => {
                matches!(
                    ln.role,
                    LineageRole::DerivedInput | LineageRole::Parsed | LineageRole::Normalized
                )
            }
            FieldKind::QualityFlag => matches!(ln.role, LineageRole::QualityInput),
        };
        if !role_ok {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: output_fields[{}] has kind={:?} but lineage role {:?} is inconsistent with mapping expectations",
                    tid, field.name, field.kind, ln.role
                ),
            ));
        }
    }
}

fn duplicate_workgroup_refs(groups: &[PlanWorkGroup]) -> Vec<(String, String)> {
    let mut counts = std::collections::BTreeMap::<(String, String), usize>::new();
    for g in groups.iter() {
        for it in g.items.iter() {
            let tid = it.task_id.trim();
            let cid = it.checklist_item_id.trim();
            if tid.is_empty() || cid.is_empty() {
                continue;
            }
            *counts
                .entry((tid.to_string(), cid.to_string()))
                .or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

fn validation_result(issues: Vec<PlanSemanticIssue>) -> PlanSemanticValidation {
    let errors: Vec<String> = issues.iter().map(|i| i.message.clone()).collect();
    PlanSemanticValidation {
        ok: errors.is_empty(),
        issues,
        errors,
    }
}

fn insert_unambiguous_case_folded_field(
    fields: &mut std::collections::BTreeMap<String, Option<String>>,
    name: &str,
) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return;
    }
    let key = trimmed.to_ascii_lowercase();
    match fields.get_mut(&key) {
        Some(existing) if existing.as_deref() != Some(trimmed) => {
            *existing = None;
        }
        Some(_) => {}
        None => {
            fields.insert(key, Some(trimmed.to_string()));
        }
    }
}

fn fill_missing_case_folded_fields_from_fallback(
    fields: &mut std::collections::BTreeMap<String, Option<String>>,
    fallback: std::collections::BTreeMap<String, Option<String>>,
) {
    for (key, value) in fallback {
        fields.entry(key).or_insert(value);
    }
}

fn normalize_model_metric_source_fields(plan: &mut ModelPlan) {
    for task in plan.tasks.iter_mut() {
        let Some(spec) = task.implementation_spec.as_mut() else {
            continue;
        };
        // Prefer grounded input schemas because they come from materialized
        // warehouse relations. `task.source_schema` and output fields are only
        // fallback contracts when no warehouse-backed name is available.
        let mut canonical_fields: std::collections::BTreeMap<String, Option<String>> =
            std::collections::BTreeMap::new();
        for input in &task.grounded_inputs {
            for column in &input.source_schema {
                insert_unambiguous_case_folded_field(&mut canonical_fields, &column.name);
            }
        }
        let mut fallback_fields: std::collections::BTreeMap<String, Option<String>> =
            std::collections::BTreeMap::new();
        for column in &task.source_schema {
            insert_unambiguous_case_folded_field(&mut fallback_fields, &column.name);
        }
        for output_field in &spec.output_fields {
            insert_unambiguous_case_folded_field(&mut fallback_fields, &output_field.name);
        }
        fill_missing_case_folded_fields_from_fallback(&mut canonical_fields, fallback_fields);

        for metric in &mut spec.metrics {
            for field in &mut metric.source_fields {
                let trimmed = field.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(Some(canonical)) = canonical_fields.get(&trimmed.to_ascii_lowercase()) {
                    *field = canonical.clone();
                }
            }
            metric.source_fields.sort();
            metric.source_fields.dedup();
        }
    }
}

fn has_safe_claim(
    spec: &crate::plan_types::ModelImplementationSpec,
    kinds: &[crate::providers::SemanticClaimKind],
) -> bool {
    spec.evidence_claim_refs.iter().any(|claim| {
        claim.status.authoring_safe()
            && claim_ref_resolves_for_validation(claim)
            && kinds.iter().any(|kind| kind == &claim.kind)
    })
}

pub(crate) fn claim_ref_resolves_for_validation(
    claim: &crate::providers::SemanticClaimRef,
) -> bool {
    use crate::providers::{EvidenceStatus, SemanticClaimKind};
    if claim.claim_id.as_str().trim().is_empty() {
        return false;
    }
    if claim.status == EvidenceStatus::UserProvided {
        return true;
    }
    let prefix = match claim.kind {
        SemanticClaimKind::CandidateKey => "candidate_key:",
        SemanticClaimKind::Relationship => "relationship:",
        SemanticClaimKind::Grain => "grain:",
        SemanticClaimKind::NumericParse => "numeric_parse:",
        SemanticClaimKind::TimeField => "time_field:",
        SemanticClaimKind::RowPreservation => "row_preservation:",
        SemanticClaimKind::AggregateSafety => "aggregate_safety:",
    };
    claim.claim_id.as_str().starts_with(prefix)
}

fn model_evidence_issues(
    tid: &str,
    spec: &crate::plan_types::ModelImplementationSpec,
) -> Vec<PlanSemanticIssue> {
    use crate::providers::SemanticClaimKind::{
        AggregateSafety, CandidateKey, Grain, NumericParse, Relationship, RowPreservation,
        TimeField,
    };
    use PlanSemanticIssueCode::UnverifiedSemanticEvidence;

    let mut issues = Vec::new();
    for claim in &spec.evidence_claim_refs {
        if !claim.status.authoring_safe() {
            issues.push(sem_task(
                UnverifiedSemanticEvidence,
                tid,
                format!(
                    "{}: evidence claim '{}' for {:?} is {:?}; model authoring requires observed or user_provided evidence",
                    tid, claim.claim_id, claim.kind, claim.status
                ),
            ));
        }
        if claim.status.authoring_safe() && !claim_ref_resolves_for_validation(claim) {
            issues.push(sem_task(
                UnverifiedSemanticEvidence,
                tid,
                format!(
                    "{}: evidence claim '{}' for {:?} does not resolve to a typed profile/user evidence id",
                    tid, claim.claim_id, claim.kind
                ),
            ));
        }
    }

    if !spec.grain.trim().is_empty() && !has_safe_claim(spec, &[Grain, CandidateKey]) {
        issues.push(sem_task(
            UnverifiedSemanticEvidence,
            tid,
            format!(
                "{}: implementation_spec.grain requires an observed/user_provided grain or candidate_key evidence_claim_ref",
                tid
            ),
        ));
    }
    if spec
        .joins
        .iter()
        .any(|join| join.cardinality.is_some() || !join.on.is_empty())
        && !has_safe_claim(spec, &[Relationship, CandidateKey])
    {
        issues.push(sem_task(
            UnverifiedSemanticEvidence,
            tid,
            format!(
                "{}: joins/cardinality require observed/user_provided relationship or candidate_key evidence_claim_ref",
                tid
            ),
        ));
    }
    if !spec.metrics.is_empty()
        && !has_safe_claim(spec, &[AggregateSafety, Grain, CandidateKey, NumericParse])
    {
        issues.push(sem_task(
            UnverifiedSemanticEvidence,
            tid,
            format!(
                "{}: metrics require observed/user_provided aggregate_safety, grain, candidate_key, or numeric_parse evidence_claim_ref",
                tid
            ),
        ));
    }
    let lower_assumptions = spec.assumptions.join(" ").to_ascii_lowercase();
    if contains_semantic_risk_word(&lower_assumptions)
        && !has_safe_claim(
            spec,
            &[
                Grain,
                CandidateKey,
                Relationship,
                NumericParse,
                TimeField,
                RowPreservation,
                AggregateSafety,
            ],
        )
    {
        issues.push(sem_task(
            UnverifiedSemanticEvidence,
            tid,
            format!(
                "{}: semantic assumptions require observed/user_provided evidence_claim_refs",
                tid
            ),
        ));
    }
    issues
}

fn contains_semantic_risk_word(text: &str) -> bool {
    [
        "unique",
        "dedup",
        "grain",
        "one row",
        "many-to-one",
        "many_to_one",
        "cardinality",
        "parse",
        "numeric",
        "row preserving",
        "row-preserving",
        "preserve rows",
        "aggregate",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Structural validation shared by all plan types.
///
/// `task_id_label` is used for human-readable error messages (e.g. "dataset_id", "name").
/// `batch_id_label` is the noun for batch id references (e.g. "dataset_id", "model name").
/// `wg_qualifier` is the noun for task references in work_group errors (e.g. "dataset_id", "model").
fn validate_plan_structure<T: PlanTask>(
    plan: &Plan<T>,
    task_id_label: &str,
    batch_id_label: &str,
    wg_qualifier: &str,
) -> Vec<PlanSemanticIssue> {
    use PlanSemanticIssueCode::*;
    let mut issues: Vec<PlanSemanticIssue> = Vec::new();
    if plan.plan_key.trim().is_empty() {
        issues.push(sem(MissingPlanKey, "plan_key is missing"));
    }
    if plan.tasks.is_empty() {
        issues.push(sem(MissingTasks, "tasks is empty"));
    }
    let task_ids: Vec<String> = plan.tasks.iter().map(|t| t.task_id().to_string()).collect();
    for dup in duplicate_values(&task_ids) {
        issues.push(sem(
            DuplicateTaskId,
            format!("duplicate task.{} is not allowed: {}", task_id_label, dup),
        ));
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        if b.len() > MAX_BATCH_SIZE {
            issues.push(sem(
                InvalidBatch,
                format!(
                    "batches[{bi}] has >{MAX_BATCH_SIZE} items (len={})",
                    b.len()
                ),
            ));
        }
        for dup in duplicate_values(b) {
            issues.push(sem(
                InvalidBatch,
                format!(
                    "batches[{bi}] contains duplicate {} '{}' (duplicates are forbidden)",
                    batch_id_label, dup
                ),
            ));
        }
    }

    for t in plan.tasks.iter() {
        let tid = t.task_id();
        if tid.trim().is_empty() {
            issues.push(sem(
                MissingTasks,
                format!("task.{} is empty", task_id_label),
            ));
            continue;
        }
        let missing = missing_required_checklist_items(t.checklist());
        if !missing.is_empty() {
            issues.push(sem_task(
                MissingChecklistItems,
                tid,
                format!(
                    "{}: missing required checklist items: {}",
                    tid,
                    missing.join(", ")
                ),
            ));
        }
        let sql_status = checklist_status(t.checklist(), CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            let missing_path = t
                .expected_model_path()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if missing_path {
                issues.push(sem_task(
                    MissingImplementationSpec,
                    tid,
                    format!("{}: expected_model_path missing for runnable task", tid),
                ));
            }
        }
    }

    for (bi, b) in plan.batches.iter().enumerate() {
        for item in b.iter() {
            if !plan.tasks.iter().any(|t| t.task_id() == item.as_str()) {
                issues.push(sem(
                    InvalidBatch,
                    format!(
                        "batches[{bi}] references {} not present in tasks: {item}",
                        batch_id_label
                    ),
                ));
            }
        }
    }

    if plan.work_groups.is_empty() {
        issues.push(sem(MissingWorkGroups, "work_groups is empty"));
    }
    for (task_id, checklist_item_id) in duplicate_workgroup_refs(&plan.work_groups) {
        issues.push(sem(MissingWorkGroupCoverage, format!(
            "work_groups contains duplicate task/checklist ref: task_id='{}' checklist_item_id='{}'",
            task_id, checklist_item_id
        )));
    }
    for g in plan.work_groups.iter() {
        if g.items.len() > MAX_BATCH_SIZE {
            issues.push(sem(
                InvalidBatch,
                format!(
                    "work_group {} has >{MAX_BATCH_SIZE} items (len={})",
                    g.group_id,
                    g.items.len()
                ),
            ));
        }
        for it in g.items.iter() {
            if !plan
                .tasks
                .iter()
                .any(|t| t.task_id() == it.task_id.as_str())
            {
                issues.push(sem(
                    UnknownTaskReference,
                    format!(
                        "work_group {} references unknown {} task_id={}",
                        g.group_id, wg_qualifier, it.task_id
                    ),
                ));
            }
            let cid = it.checklist_item_id.trim();
            if cid.is_empty() {
                issues.push(sem(
                    MissingChecklistItems,
                    format!(
                        "work_group {} item for task_id={} is missing checklist_item_id",
                        g.group_id, it.task_id
                    ),
                ));
                continue;
            }
            if let Some(t) = plan
                .tasks
                .iter()
                .find(|t| t.task_id() == it.task_id.as_str())
            {
                let exists = t.checklist().iter().any(|x| x.checklist_item_id == cid);
                if !exists {
                    issues.push(sem(UnknownTaskReference, format!(
                        "work_group {} references checklist_item_id '{}' not present in task checklist: {}",
                        g.group_id, cid, it.task_id
                    )));
                }
            }
        }
    }
    for t in plan.tasks.iter() {
        for checklist_id in required_checklist_item_ids() {
            if !work_groups_cover_task_checklist(&plan.work_groups, t.task_id(), checklist_id) {
                issues.push(sem_task(
                    MissingWorkGroupCoverage,
                    t.task_id(),
                    format!(
                        "task {} checklist '{}' is not scheduled in work_groups",
                        t.task_id(),
                        checklist_id
                    ),
                ));
            }
        }
    }
    issues
}

pub fn validate_cleanse_plan_semantics(plan: &CleansePlan) -> PlanSemanticValidation {
    use PlanSemanticIssueCode::*;
    let mut issues = validate_plan_structure(plan, "dataset_id", "dataset_id", "dataset_id");

    for t in plan.tasks.iter() {
        let tid = &t.dataset_id;
        if tid.trim().is_empty() {
            continue;
        }
        let Some(spec) = t.implementation_spec.as_ref() else {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!("{}: missing implementation_spec", tid),
            ));
            continue;
        };
        if spec.spec_version <= 0 {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!("{}: implementation_spec.spec_version must be >0", tid),
            ));
        }
        if !spec.row_preserving {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: implementation_spec.row_preserving must be true for cleanse/silver",
                    tid
                ),
            ));
        }
        if spec.output_fields.is_empty() {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: implementation_spec.output_fields is empty (design detail required)",
                    tid
                ),
            ));
        }
        let known: std::collections::BTreeSet<String> =
            t.source_schema.iter().map(|c| c.name.clone()).collect();
        if !known.is_empty() {
            for field in &spec.output_fields {
                for sc in &field.source_columns {
                    if !known.contains(sc.as_str()) {
                        issues.push(sem_task(MissingImplementationSpec, tid, format!(
                            "{}: output_fields[{}].source_columns references '{}' which is not in the task's authoritative source_schema (available: {})",
                            tid,
                            field.name,
                            sc,
                            known.iter().cloned().collect::<Vec<_>>().join(", ")
                        )));
                    }
                }
            }
        }
        for field in &spec.output_fields {
            validate_output_field_lineage_cleanse(tid, field, &known, &mut issues);
        }
    }
    validation_result(issues)
}

pub fn validate_model_plan_semantics(
    plan: &ModelPlan,
    allowed_staging_models: Option<&std::collections::BTreeSet<String>>,
) -> PlanSemanticValidation {
    use PlanSemanticIssueCode::*;
    let mut issues = validate_plan_structure(plan, "name", "model name", "model");

    let plan_task_names: std::collections::BTreeSet<&str> = plan
        .tasks
        .iter()
        .map(|t| t.name.trim())
        .filter(|s| !s.is_empty())
        .collect();

    for t in plan.tasks.iter() {
        let tid = &t.name;
        if tid.trim().is_empty() {
            continue;
        }
        let Some(spec) = t.implementation_spec.as_ref() else {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!("{}: missing implementation_spec", tid),
            ));
            continue;
        };
        if spec.spec_version <= 0 {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!("{}: implementation_spec.spec_version must be >0", tid),
            ));
        }
        if spec.grain.trim().is_empty() {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: implementation_spec.grain is empty (design detail required)",
                    tid
                ),
            ));
        }
        if spec.output_fields.is_empty() {
            issues.push(sem_task(
                MissingImplementationSpec,
                tid,
                format!(
                    "{}: implementation_spec.output_fields is empty (gold SQL requires an explicit output contract)",
                    tid
                ),
            ));
        }
        let output_field_names: std::collections::BTreeSet<String> = spec
            .output_fields
            .iter()
            .map(|f| f.name.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let source_field_names: std::collections::BTreeSet<String> = t
            .source_schema
            .iter()
            .map(|c| c.name.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let available_metric_fields: std::collections::BTreeSet<String> = source_field_names
            .iter()
            .cloned()
            .chain(output_field_names.iter().cloned())
            .chain(t.grounded_inputs.iter().flat_map(|input| {
                input
                    .source_schema
                    .iter()
                    .map(|c| c.name.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            }))
            .collect();
        let mut per_input: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for gi in &t.grounded_inputs {
            let cols: std::collections::BTreeSet<String> = gi
                .source_schema
                .iter()
                .map(|c| c.name.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            per_input.insert(gi.input_name.trim().to_string(), cols);
        }
        for field in &spec.output_fields {
            validate_output_field_lineage_model(
                tid,
                field,
                &per_input,
                &source_field_names,
                &mut issues,
            );
        }
        for metric in &spec.metrics {
            let metric_id = if metric.name.trim().is_empty() {
                "<unnamed>"
            } else {
                metric.name.trim()
            };
            if metric.source_fields.is_empty() {
                issues.push(sem_task(
                    MissingImplementationSpec,
                    tid,
                    format!(
                        "{}: metric '{}' must list source_fields used by its definition",
                        tid, metric_id
                    ),
                ));
            }
            for field in &metric.source_fields {
                let f = field.trim();
                if f.is_empty() {
                    continue;
                }
                if !available_metric_fields.contains(f) {
                    issues.push(sem_task(MissingImplementationSpec, tid, format!(
                        "{}: metric '{}'.source_fields references '{}' which is not in source_schema, output_fields, or grounded input schemas",
                        tid, metric_id, f
                    )));
                }
            }
        }
        issues.extend(model_evidence_issues(tid, spec));
        let sql_status = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            if t.goal.trim().is_empty() {
                issues.push(sem_task(
                    MissingImplementationSpec,
                    tid,
                    format!("{}: goal is empty for runnable task", tid),
                ));
            }
            let nonempty_inputs: Vec<String> = t
                .inputs
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if nonempty_inputs.is_empty() {
                issues.push(sem_task(
                    MissingImplementationSpec,
                    tid,
                    format!("{}: inputs is empty for runnable task", tid),
                ));
            }
            let grounded_input_names: std::collections::BTreeSet<String> = t
                .grounded_inputs
                .iter()
                .map(|g| g.input_name.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            for inp in nonempty_inputs.iter() {
                if !grounded_input_names.contains(inp) {
                    issues.push(sem_task(MissingImplementationSpec, tid, format!(
                        "{}: grounded_inputs is missing authoritative relation facts for input '{}'",
                        tid, inp
                    )));
                }
            }
            for grounded in &t.grounded_inputs {
                if grounded.relation_fqn.trim().is_empty() {
                    issues.push(sem_task(
                        MissingImplementationSpec,
                        tid,
                        format!(
                            "{}: grounded_inputs[{}].relation_fqn is empty",
                            tid, grounded.input_name
                        ),
                    ));
                }
                if grounded.model_rel_path.trim().is_empty() {
                    issues.push(sem_task(
                        MissingImplementationSpec,
                        tid,
                        format!(
                            "{}: grounded_inputs[{}].model_rel_path is empty",
                            tid, grounded.input_name
                        ),
                    ));
                }
                if grounded.source_schema.is_empty() {
                    tracing::warn!(
                        "{}: grounded_inputs[{}].source_schema is empty (warehouse schema not yet available)",
                        tid, grounded.input_name
                    );
                }
            }
            if let Some(allowed) = allowed_staging_models {
                for inp in nonempty_inputs.iter() {
                    if !allowed.contains(inp) && !plan_task_names.contains(inp.as_str()) {
                        issues.push(sem_task(
                            UnknownTaskReference,
                            tid,
                            format!(
                                "{}: input '{}' not grounded in known staging models or plan tasks",
                                tid, inp
                            ),
                        ));
                    }
                }
            }
        }
    }
    validation_result(issues)
}

pub fn ensure_cleanse_plan_semantically_valid_or_repaired(
    plan: &mut CleansePlan,
) -> PlanSemanticValidation {
    crate::plan_grounding::normalize_cleanse_plan_defaults(plan);
    validate_cleanse_plan_semantics(plan)
}

pub fn ensure_model_plan_semantically_valid_or_repaired(
    plan: &mut ModelPlan,
    allowed_staging_models: &std::collections::BTreeSet<String>,
) -> PlanSemanticValidation {
    crate::plan_grounding::ensure_expected_model_paths_model(plan);
    crate::plan_grounding::materialize_output_lineage_from_legacy_model(plan);
    normalize_model_metric_source_fields(plan);
    validate_model_plan_semantics(plan, Some(allowed_staging_models))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_progress::canonical_task_checklist;
    use crate::plan_types::*;
    use crate::providers::{EvidenceStatus, SemanticClaimKind, SemanticClaimRef};
    use crate::track_spec::TrackKind;

    fn model_plan_with_claims(claims: Vec<SemanticClaimRef>) -> ModelPlan {
        let mut plan = ModelPlan {
            plan_key: "model:test".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "dim_customers".to_string(),
                folder: ModelFolder::Marts,
                goal: "Customer dimension".to_string(),
                inputs: vec!["stg_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer_id".to_string(),
                    inputs: vec!["stg_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: FieldKind::Clean,
                        lineage: vec![],
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: false,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: claims,
                }),
                source_schema: vec![SourceColumnDef {
                    name: "customer_id".to_string(),
                    data_type: "string".to_string(),
                }],
                grounded_inputs: vec![GroundedModelInput {
                    input_name: "stg_customers".to_string(),
                    model_rel_path: "models/staging/stg_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_customers".to_string(),
                    source_schema: vec![SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: TaskStatus::Pending,
                checklist: canonical_task_checklist(TrackKind::Model),
            }],
            batches: vec![vec!["dim_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: Default::default(),
        };
        plan.reconcile_work_groups();
        plan
    }

    #[test]
    fn model_plan_blocks_grain_without_safe_evidence() {
        let plan = model_plan_with_claims(vec![]);
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = validate_model_plan_semantics(&plan, Some(&allowed));

        assert!(!result.ok);
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == PlanSemanticIssueCode::UnverifiedSemanticEvidence));
    }

    #[test]
    fn model_plan_accepts_grain_with_observed_evidence() {
        let plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = validate_model_plan_semantics(&plan, Some(&allowed));

        assert!(result.ok, "{:?}", result.errors);
    }

    #[test]
    fn model_plan_requires_explicit_output_fields() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .output_fields
            .clear();
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = validate_model_plan_semantics(&plan, Some(&allowed));

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("output_fields is empty")));
    }

    #[test]
    fn model_plan_requires_metric_source_fields() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .metrics
            .push(MetricSpec {
                name: "revenue".to_string(),
                definition: "sum(total_amount)".to_string(),
                source_fields: vec![],
                caveats: vec![],
            });
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = validate_model_plan_semantics(&plan, Some(&allowed));

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("must list source_fields")));
    }

    #[test]
    fn model_plan_allows_metric_source_fields_from_grounded_inputs() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0].source_schema.clear();
        plan.tasks[0].grounded_inputs[0].source_schema = vec![
            SourceColumnDef {
                name: "customer_id".to_string(),
                data_type: "string".to_string(),
            },
            SourceColumnDef {
                name: "order_id".to_string(),
                data_type: "number".to_string(),
            },
        ];
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .metrics
            .push(MetricSpec {
                name: "order_count".to_string(),
                definition: "count(order_id)".to_string(),
                source_fields: vec!["order_id".to_string()],
                caveats: vec![],
            });
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = validate_model_plan_semantics(&plan, Some(&allowed));

        assert!(result.ok, "{:?}", result.errors);
    }

    #[test]
    fn model_plan_repairs_metric_source_field_casing_before_validation() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0].source_schema.clear();
        plan.tasks[0].grounded_inputs[0].source_schema = vec![
            SourceColumnDef {
                name: "customer_id".to_string(),
                data_type: "string".to_string(),
            },
            SourceColumnDef {
                name: "order_id".to_string(),
                data_type: "number".to_string(),
            },
        ];
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .metrics
            .push(MetricSpec {
                name: "order_count".to_string(),
                definition: "count(order_id)".to_string(),
                source_fields: vec!["ORDER_ID".to_string()],
                caveats: vec![],
            });
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = ensure_model_plan_semantically_valid_or_repaired(&mut plan, &allowed);

        assert!(result.ok, "{:?}", result.errors);
        assert_eq!(
            plan.tasks[0]
                .implementation_spec
                .as_ref()
                .expect("spec")
                .metrics[0]
                .source_fields,
            vec!["order_id".to_string()]
        );
    }

    #[test]
    fn model_plan_repairs_metric_source_field_to_warehouse_canonical_casing() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0].source_schema = vec![
            SourceColumnDef {
                name: "customer_id".to_string(),
                data_type: "string".to_string(),
            },
            SourceColumnDef {
                name: "order_id".to_string(),
                data_type: "number".to_string(),
            },
        ];
        plan.tasks[0].grounded_inputs[0].source_schema = vec![
            SourceColumnDef {
                name: "customer_id".to_string(),
                data_type: "STRING".to_string(),
            },
            SourceColumnDef {
                name: "ORDER_ID".to_string(),
                data_type: "NUMBER".to_string(),
            },
        ];
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .metrics
            .push(MetricSpec {
                name: "order_count".to_string(),
                definition: "count(order_id)".to_string(),
                source_fields: vec!["order_id".to_string()],
                caveats: vec![],
            });
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = ensure_model_plan_semantically_valid_or_repaired(&mut plan, &allowed);

        assert!(result.ok, "{:?}", result.errors);
        assert_eq!(
            plan.tasks[0]
                .implementation_spec
                .as_ref()
                .expect("spec")
                .metrics[0]
                .source_fields,
            vec!["ORDER_ID".to_string()]
        );
    }

    #[test]
    fn model_plan_still_rejects_unknown_metric_source_fields() {
        let mut plan = model_plan_with_claims(vec![SemanticClaimRef {
            claim_id: "candidate_key:db.schema.customers:customer_id"
                .to_string()
                .into(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        }]);
        plan.tasks[0].source_schema.clear();
        plan.tasks[0].grounded_inputs[0].source_schema = vec![SourceColumnDef {
            name: "ORDER_ID".to_string(),
            data_type: "NUMBER".to_string(),
        }];
        plan.tasks[0]
            .implementation_spec
            .as_mut()
            .expect("spec")
            .metrics
            .push(MetricSpec {
                name: "missing".to_string(),
                definition: "sum(missing_field)".to_string(),
                source_fields: vec!["MISSING_FIELD".to_string()],
                caveats: vec![],
            });
        let allowed = ["stg_customers".to_string()].into_iter().collect();

        let result = ensure_model_plan_semantically_valid_or_repaired(&mut plan, &allowed);

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("MISSING_FIELD")));
    }

    fn minimal_cleanse_plan(
        dataset_id: &str,
        source_schema: Vec<SourceColumnDef>,
        output_fields: Vec<OutputFieldSpec>,
    ) -> CleansePlan {
        let dataset_id = dataset_id.to_string();
        let batches = vec![vec![dataset_id.clone()]];
        CleansePlan {
            plan_key: "cleanse:test_lineage".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: dataset_id.clone(),
                expected_model_path: Some("models/staging/stg_test.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields,
                    prohibited_ops: vec![],
                }),
                source_schema,
                status: TaskStatus::Pending,
                checklist: canonical_task_checklist(TrackKind::Cleanse),
            }],
            batches: batches.clone(),
            work_groups: crate::plan_progress::canonical_work_groups_from_batches(
                &batches, "cleanse",
            ),
            mutations: vec![],
            progress: Default::default(),
        }
    }

    #[test]
    fn cleanse_accepts_dotted_source_mapped_to_flat_outputs_via_lineage() {
        let schema = vec![
            SourceColumnDef {
                name: "context.session.id".to_string(),
                data_type: "string".to_string(),
            },
            SourceColumnDef {
                name: "event_type".to_string(),
                data_type: "string".to_string(),
            },
        ];
        let fields = vec![
            OutputFieldSpec {
                name: "session_id".to_string(),
                kind: FieldKind::Clean,
                lineage: vec![FieldLineage {
                    source: SourceFieldRef {
                        relation: None,
                        name: "context.session.id".to_string(),
                    },
                    role: LineageRole::Normalized,
                }],
                source_columns: vec![],
                expression: "cast session as varchar".to_string(),
                data_type: None,
                nullable: true,
                description: None,
            },
            OutputFieldSpec {
                name: "event_type".to_string(),
                kind: FieldKind::Raw,
                lineage: vec![FieldLineage {
                    source: SourceFieldRef {
                        relation: None,
                        name: "event_type".to_string(),
                    },
                    role: LineageRole::Passthrough,
                }],
                source_columns: vec![],
                expression: "passthrough".to_string(),
                data_type: None,
                nullable: false,
                description: None,
            },
        ];
        let plan = minimal_cleanse_plan("ds.events", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(r.ok, "{:?}", r.errors);
    }

    #[test]
    fn cleanse_rejects_publishing_dotted_source_name_as_clean_field() {
        let schema = vec![SourceColumnDef {
            name: "context.session.id".to_string(),
            data_type: "string".to_string(),
        }];
        let fields = vec![OutputFieldSpec {
            name: "context.session.id".to_string(),
            kind: FieldKind::Clean,
            lineage: vec![FieldLineage {
                source: SourceFieldRef {
                    relation: None,
                    name: "context.session.id".to_string(),
                },
                role: LineageRole::Normalized,
            }],
            source_columns: vec![],
            expression: "oops".to_string(),
            data_type: None,
            nullable: true,
            description: None,
        }];
        let plan = minimal_cleanse_plan("ds.events", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("kind=raw")));
    }

    #[test]
    fn cleanse_accepts_dotted_published_name_when_raw_passthrough() {
        let schema = vec![SourceColumnDef {
            name: "context.session.id".to_string(),
            data_type: "string".to_string(),
        }];
        let fields = vec![OutputFieldSpec {
            name: "context.session.id".to_string(),
            kind: FieldKind::Raw,
            lineage: vec![FieldLineage {
                source: SourceFieldRef {
                    relation: None,
                    name: "context.session.id".to_string(),
                },
                role: LineageRole::Passthrough,
            }],
            source_columns: vec![],
            expression: "passthrough".to_string(),
            data_type: None,
            nullable: true,
            description: None,
        }];
        let plan = minimal_cleanse_plan("ds.events", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(r.ok, "{:?}", r.errors);
    }

    #[test]
    fn cleanse_rejects_orphan_output_without_lineage_or_sources() {
        let schema = vec![SourceColumnDef {
            name: "a".to_string(),
            data_type: "string".to_string(),
        }];
        let fields = vec![OutputFieldSpec {
            name: "orphan".to_string(),
            kind: FieldKind::Derived,
            lineage: vec![],
            source_columns: vec![],
            expression: "constant".to_string(),
            data_type: None,
            nullable: true,
            description: None,
        }];
        let plan = minimal_cleanse_plan("ds.t", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("missing lineage")));
    }

    #[test]
    fn cleanse_legacy_source_columns_derives_lineage_for_validation() {
        let schema = vec![SourceColumnDef {
            name: "event_ts".to_string(),
            data_type: "timestamp".to_string(),
        }];
        let fields = vec![OutputFieldSpec {
            name: "event_ts_clean".to_string(),
            kind: FieldKind::Clean,
            lineage: vec![],
            source_columns: vec!["event_ts".to_string()],
            expression: "cast to timestamptz".to_string(),
            data_type: None,
            nullable: false,
            description: None,
        }];
        let plan = minimal_cleanse_plan("ds.t", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(r.ok, "{:?}", r.errors);
    }

    #[test]
    fn cleanse_rejects_raw_with_multiple_legacy_source_columns_without_lineage() {
        let schema = vec![
            SourceColumnDef {
                name: "a".to_string(),
                data_type: "string".to_string(),
            },
            SourceColumnDef {
                name: "b".to_string(),
                data_type: "string".to_string(),
            },
        ];
        let fields = vec![OutputFieldSpec {
            name: "x".to_string(),
            kind: FieldKind::Raw,
            lineage: vec![],
            source_columns: vec!["a".to_string(), "b".to_string()],
            expression: "composite".to_string(),
            data_type: None,
            nullable: true,
            description: None,
        }];
        let plan = minimal_cleanse_plan("ds.t", schema, fields);
        let r = validate_cleanse_plan_semantics(&plan);
        assert!(!r.ok);
        assert!(r
            .errors
            .iter()
            .any(|e| e.contains("multiple source_columns")));
    }

    #[test]
    fn output_field_spec_deserializes_legacy_json_without_lineage_key() {
        let json = r#"{
            "name": "id",
            "kind": "clean",
            "source_columns": ["id"],
            "expression": "cast",
            "nullable": false
        }"#;
        let f: OutputFieldSpec = serde_json::from_str(json).expect("parse");
        assert!(f.lineage.is_empty());
        assert_eq!(f.effective_lineage().len(), 1);
        assert_eq!(f.effective_lineage()[0].role, LineageRole::Normalized);
    }
}
