// Declared as `mod review_persistence;` in data_engineer/mod.rs

use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::session::{Observation, ThreadStep, ThreadStore};

use super::control_flow::Phase;
use super::domain_types::{ReviewDecision, ReviewTier};
use super::plan as de_plan;
use super::plan_kind::PlanKind;
use super::review_batched::{utc_ts, MAX_BATCHES_SAVED, REVIEW_SNAPSHOT_VERSION};

pub(super) async fn append_review_step(
    store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    agent: &str,
    reason_code_str: &str,
    detail: Value,
) -> Result<(), String> {
    store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: phase.as_str().to_string(),
                from_phase: Some(phase.as_str().to_string()),
                reason_code: Some(reason_code_str.to_string()),
                reason_detail: Some(detail),
                observation: Observation::ok(),
                ts: utc_ts(),
                agent: agent.to_string(),
            },
        )
        .await
        .map_err(|e| format!("failed to append review step: {e}"))?;
    Ok(())
}

fn upsert_review_snapshot(obj: &mut serde_json::Map<String, Value>, patch: Value) {
    let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
    if review.is_null() {
        *review = serde_json::json!({});
    }
    let review_obj = review
        .as_object_mut()
        .expect("review must be a JSON object");
    if let Some(patch_obj) = patch.as_object() {
        for (k, v) in patch_obj.iter() {
            review_obj.insert(k.clone(), v.clone());
        }
    }
    review_obj.insert(
        "review_version".to_string(),
        serde_json::json!(REVIEW_SNAPSHOT_VERSION),
    );
}

fn push_batch_entry(review_obj: &mut serde_json::Map<String, Value>, entry: Value) {
    let batches = review_obj
        .entry("batches")
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    let Some(arr) = batches.as_array_mut() else {
        return;
    };
    arr.push(entry);
    while arr.len() > MAX_BATCHES_SAVED {
        arr.remove(0);
    }
}

async fn mutate_plan_review(
    actx: &AgentCtx,
    plan_kind: PlanKind,
    plan_key: &str,
    mutate_snapshot: impl Fn(&mut serde_json::Map<String, Value>),
    label: &str,
) -> Result<(), String> {
    if plan_kind == PlanKind::Cleanse {
        if let Some(mut p) = de_plan::load_cleanse_plan_by_key(actx, plan_key)
            .await
            .map_err(|e| e.to_string())?
        {
            mutate_snapshot(&mut p.project_snapshot.extra);
            de_plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist cleanse {label}: {e}"))?;
        }
    } else if plan_kind == PlanKind::Model {
        if let Some(mut p) = de_plan::load_model_plan_by_key(actx, plan_key)
            .await
            .map_err(|e| e.to_string())?
        {
            mutate_snapshot(&mut p.project_snapshot.extra);
            de_plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist model {label}: {e}"))?;
        }
    }
    Ok(())
}

pub(super) async fn persist_review_summary_to_plan(
    actx: &AgentCtx,
    phase: Phase,
    plan_kind: PlanKind,
    plan_key: &str,
    project_notes: Vec<String>,
) -> Result<(), String> {
    let ts = utc_ts();
    mutate_plan_review(
        actx,
        plan_kind,
        plan_key,
        |obj| {
            upsert_review_snapshot(
                obj,
                serde_json::json!({
                    "phase": phase.as_str(),
                    "project_notes": project_notes,
                    "ts": ts,
                }),
            );
        },
        "review summary",
    )
    .await
}

pub(super) async fn persist_review_batch_to_plan(
    actx: &AgentCtx,
    plan_kind: PlanKind,
    plan_key: &str,
    batch_idx: usize,
    batch_items: Vec<String>,
    findings: Vec<String>,
) -> Result<(), String> {
    let ts = utc_ts();
    let entry = serde_json::json!({
        "batch_idx": batch_idx,
        "batch_items": batch_items,
        "findings": findings,
        "ts": ts,
    });
    mutate_plan_review(
        actx,
        plan_kind,
        plan_key,
        |obj| {
            let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
            if review.is_null() {
                *review = serde_json::json!({});
            }
            let review_obj = review
                .as_object_mut()
                .expect("review must be a JSON object");
            review_obj.insert(
                "review_version".to_string(),
                serde_json::json!(REVIEW_SNAPSHOT_VERSION),
            );
            push_batch_entry(review_obj, entry.clone());
        },
        "review batch",
    )
    .await
}

pub(super) async fn persist_review_final_to_plan(
    actx: &AgentCtx,
    plan_kind: PlanKind,
    plan_key: &str,
    decision: ReviewDecision,
    tier: ReviewTier,
    target_task_ids: Vec<String>,
    text: String,
) -> Result<(), String> {
    let ts = utc_ts();
    mutate_plan_review(
        actx,
        plan_kind,
        plan_key,
        |obj| {
            let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
            if review.is_null() {
                *review = serde_json::json!({});
            }
            let review_obj = review
                .as_object_mut()
                .expect("review must be a JSON object");
            review_obj.insert(
                "review_version".to_string(),
                serde_json::json!(REVIEW_SNAPSHOT_VERSION),
            );
            review_obj.insert(
                "result".to_string(),
                serde_json::json!({
                    "decision": decision,
                    "tier": tier,
                    "target_task_ids": target_task_ids,
                    "text": text,
                    "ts": ts
                }),
            );
        },
        "review result",
    )
    .await
}
