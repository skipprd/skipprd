use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use once_cell::sync::OnceCell;
use dashmap::{DashMap, DashSet};
use std::time::{Duration, Instant};

pub async fn pre_register_all_namespaces(ctx: &SessionContext) {
	let pipelines = crate::sql::registry::list_pipelines().await;
	for pipeline in pipelines {
		let mut namespaces = crate::sql::registry::list_namespaces(&pipeline).await;
		namespaces.sort();
		for ns in namespaces {
			let _ = crate::sql::tables::register_namespace_view(ctx, &pipeline, &ns).await;
		}
		let _ = crate::sql::tables::register_deadletters(ctx, &pipeline).await;
	}
}

// Process-wide caches to avoid redundant registrations
#[derive(Clone)]
struct EtagEntry { etag: String, checked_at: Instant }
static NS_ETAG_CACHE: OnceCell<DashMap<String, EtagEntry>> = OnceCell::new();
static NS_REGISTERED: OnceCell<DashSet<String>> = OnceCell::new();

fn ns_etag_cache() -> &'static DashMap<String, EtagEntry> {
	NS_ETAG_CACHE.get_or_init(|| DashMap::new())
}
fn ns_registered() -> &'static DashSet<String> {
	NS_REGISTERED.get_or_init(|| DashSet::new())
}

// Thread-scoped SessionContext reuse
static THREAD_CTX: OnceCell<DashMap<String, SessionContext>> = OnceCell::new();
pub fn get_or_create_thread_ctx(thread_id: &str) -> SessionContext {
	let map = THREAD_CTX.get_or_init(|| DashMap::new());
	if let Some(existing) = map.get(thread_id) {
		return existing.value().clone();
	}
	let ctx = SessionContext::new();
	map.insert(thread_id.to_string(), ctx.clone());
	ctx
}

// Greedy, checksum-aware registrar. Never deregisters.
pub async fn pre_register_selected_namespaces(ctx: &SessionContext, pairs: &[(String, String)]) {
	for (pipeline, namespace) in pairs.iter() {
		let key = format!("{}.{}", pipeline, namespace);
		// Resolve manifest key for this (pipeline, namespace) without using any global pipeline state
		let manifest_key = crate::sql::registry::manifest_key_for(pipeline, namespace);

		// Determine current etag (with 10-minute TTL on HEAD checks)
		let mut current_etag: String = "missing".to_string();
		let refresh_needed = match ns_etag_cache().get(&key) {
			Some(cached) => cached.checked_at.elapsed() >= Duration::from_secs(600),
			None => true,
		};
		if refresh_needed {
			match crate::helpers::s3::head_etag(&manifest_key).await {
				Ok(Some(et)) => { current_etag = et; }
				Ok(None) => { current_etag = "missing".to_string(); }
				Err(_e) => {
					// keep "missing" sentinel; avoid blocking registration on HEAD failures
				}
			}
			ns_etag_cache().insert(key.clone(), EtagEntry { etag: current_etag.clone(), checked_at: Instant::now() });
		} else if let Some(cached) = ns_etag_cache().get(&key) {
			current_etag = cached.etag.clone();
		}

		// If never registered or manifest ETag changed, (re)register
		let changed = match ns_etag_cache().get(&key) {
			Some(cached) => cached.etag != current_etag,
			None => true,
		};
		if changed || !ns_registered().contains(&key) {
			let _ = crate::sql::tables::register_namespace_view(ctx, pipeline, namespace).await;
			ns_etag_cache().insert(key.clone(), EtagEntry { etag: current_etag.clone(), checked_at: Instant::now() });
			ns_registered().insert(key.clone());
		}
	}
}

pub fn build_registry(agent: &str, ctx: &SessionContext) -> ToolRegistry {
	use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool, ask_user::AskUserTool};
	use crate::qa::tools::{approve_save::ApproveAndSaveArtifactTool, artifacts::ArtifactsTool};
	use crate::qa::tools::ask_approval::AskApprovalTool;
	let mut registry = ToolRegistry::new();
	match agent {
		"cleanse" => {
			registry.register(SqlRunTool { ctx: ctx.clone() });
			registry.register(SqlSchemaTool { ctx: ctx.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
			registry.register(ArtifactsTool);
		}
		"model" => {
			registry.register(SqlRunTool { ctx: ctx.clone() });
			registry.register(SqlSchemaTool { ctx: ctx.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
			registry.register(AskApprovalTool);
			registry.register(ApproveAndSaveArtifactTool);
			registry.register(ArtifactsTool);
		}
		_ => {
			registry.register(SqlRunTool { ctx: ctx.clone() });
			registry.register(SqlSchemaTool { ctx: ctx.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
			registry.register(ArtifactsTool);
		}
	}
	registry
}

pub fn inject_agent_question(agent: &str, question: &str) -> String {
	if agent == "model" {
		format!(
			"Modeling goal: {}.\n\
			 Work on ONE artifact at a time (either MetricFlow YAML or a DBT model SQL).\n\
			 - Use a stable logical name `name` that will never change.\n\
			 - Prefer existing artifacts if relevant (use artifacts tool); otherwise propose a new one.\n\
			 - Before asking for criteria, exhaust schema exploration: use vect_query(scope:\"field\"), sql_schema, sql_sample/sql_stats to infer plausible fields/values.\n\
			 - Propose a reasonable default filter using discovered fields (only ask if multiple equally plausible options remain).\n\
			 - Use ask_approval to request approval; use ask_user for clarifications/edits.\n\
			 - For updates: call approve_and_save_artifact with preview_diff=true first and show the diff for approval.\n\
			 - On approval: call approve_and_save_artifact with {{kind, name, content}} to save.\n\
			 - Do NOT answer with a query; your job here is artifact authoring.\n\
			 Return a compact summary only in final.",
			question
		)
	} else {
		question.to_string()
	}
}


