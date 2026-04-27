use react_core::agent::AgentCtx;

use crate::plan::NextWorkItemCtx;
use crate::track_spec::TrackKind;

pub(super) fn bind_execution_context(
    actx: &mut AgentCtx,
    track: TrackKind,
    plan_key: String,
    next_item: Option<NextWorkItemCtx>,
) {
    actx.set_exec_ctx(Some({
        let mut ctx = react_core::session::ExecutionContext::default();
        ctx.set(
            "plan_kind",
            serde_json::Value::String(track.execution_plan_kind().0),
        );
        ctx.set("plan_key", serde_json::Value::String(plan_key));
        if let Some(ref x) = next_item {
            ctx.set(
                "workgroup_id",
                serde_json::Value::String(x.workgroup_id.clone()),
            );
            ctx.set("task_id", serde_json::Value::String(x.task_id.clone()));
            ctx.set(
                "checklist_item_id",
                serde_json::Value::String(x.checklist_item_id.clone()),
            );
        }
        ctx
    }));
}

pub(super) fn collect_expected_paths<T>(
    tasks: &[T],
    ids: &[String],
    id_for_task: impl Fn(&T) -> &str,
    expected_model_path: impl Fn(&T) -> Option<&str>,
) -> Vec<String> {
    let mut expected_paths: Vec<String> = Vec::new();
    for id in ids.iter() {
        if let Some(task) = tasks.iter().find(|t| id_for_task(t) == id) {
            if let Some(path) = expected_model_path(task).map(str::trim) {
                if !path.is_empty() {
                    expected_paths.push(path.to_string());
                }
            }
        }
    }
    expected_paths.sort();
    expected_paths.dedup();
    expected_paths
}
