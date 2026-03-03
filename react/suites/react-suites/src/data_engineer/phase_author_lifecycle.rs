use react_core::agent::AgentCtx;

use crate::data_engineer::plan::NextWorkItemCtx;
use crate::data_engineer::track_spec::TrackKind;

pub(super) fn bind_execution_context(
    actx: &mut AgentCtx,
    track: TrackKind,
    plan_key: String,
    next_item: Option<NextWorkItemCtx>,
) {
    actx.exec_ctx = Some(react_core::session::ExecutionContext {
        plan_kind: Some(track.execution_plan_kind()),
        plan_key: Some(plan_key),
        workgroup_id: next_item.as_ref().map(|x| x.workgroup_id.clone()),
        task_id: next_item.as_ref().map(|x| x.task_id.clone()),
        checklist_item_id: next_item.as_ref().map(|x| x.checklist_item_id.clone()),
        data: std::collections::BTreeMap::new(),
    });
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
