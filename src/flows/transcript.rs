pub async fn append_step(thread_id: &str, action: &str, args: serde_json::Value, observation: serde_json::Value, agent: Option<String>) {
	let store = crate::qa::session::ThreadStore::new();
	let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
		action: action.to_string(),
		args,
		observation,
		ts: chrono::Utc::now().to_rfc3339(),
		agent,
	}).await;
}

pub async fn append_approval_signal(thread_id: &str, approved: bool) {
	let store = crate::qa::session::ThreadStore::new();
	let text = if approved { "Approved" } else { "Rejected" };
	let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
		action: "user".to_string(),
		args: serde_json::json!({ "text": text }),
		observation: serde_json::json!({"ok": true}),
		ts: chrono::Utc::now().to_rfc3339(),
		agent: None,
	}).await;
}


