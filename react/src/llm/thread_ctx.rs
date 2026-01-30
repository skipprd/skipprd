use tokio::task_local;

task_local! {
    static LLM_THREAD_ID: String;
}

/// Scope all LLM calls within `fut` to this thread id (task-local).
pub async fn scope_thread_id<F, R>(thread_id: &str, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    LLM_THREAD_ID.scope(thread_id.to_string(), fut).await
}

/// Best-effort: returns the current task-local thread id, if set.
pub fn current_thread_id() -> Option<String> {
    LLM_THREAD_ID.try_with(|s| s.clone()).ok()
}

