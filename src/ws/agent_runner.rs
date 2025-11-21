use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use once_cell::sync::OnceCell;
use dashmap::{DashMap, DashSet};
use std::time::{Duration, Instant};
use futures::{stream::FuturesUnordered, StreamExt};
use std::pin::Pin;
use std::future::Future;

pub async fn pre_register_all_namespaces(ctx: &SessionContext) {
	let pipelines = crate::sql::registry::list_pipelines().await;
	// Bounded concurrency
	let limit = 16usize;
	let mut futs: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>> = FuturesUnordered::new();
	for pipeline in pipelines {
		let mut namespaces = crate::sql::registry::list_namespaces(&pipeline).await;
		namespaces.sort();
		for ns in namespaces {
			// Backpressure
			while futs.len() >= limit {
				let _ = futs.next().await;
			}
			let ctx2 = ctx.clone();
			let p2 = pipeline.clone();
			futs.push(Box::pin(async move {
				let _ = crate::sql::tables::register_namespace_view(&ctx2, &p2, &ns).await;
			}));
		}
		let ctx3 = ctx.clone();
		let p3 = pipeline.clone();
		futs.push(Box::pin(async move {
			let _ = crate::sql::tables::register_deadletters(&ctx3, &p3).await;
		}));
	}
	while let Some(_) = futs.next().await {}
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
			registry.register(crate::qa::tools::approve_save_batch::ApproveAndSaveArtifactBatchTool);
			registry.register(crate::qa::tools::dbt_examples::SearchDbtExamplesTool);
			registry.register(crate::qa::tools::dbt_validate::DbtValidateTool);
			registry.register(crate::qa::tools::sql_register::SqlRegisterTool);
			registry.register(crate::qa::tools::catalog_note::CatalogNoteTool);
			registry.register(ArtifactsTool);
		}
		_ => {
			registry.register(SqlRunTool { ctx: ctx.clone() });
			registry.register(SqlSchemaTool { ctx: ctx.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx.clone() });
			registry.register(VectQueryTool);
			registry.register(ArtifactsTool);
		}
	}
	registry
}

pub fn inject_agent_question(agent: &str, question: &str) -> String {
	if agent == "model" {
		format!(
			"Modeling goal: {}.\n\
			 Act as a proactive DBT Engineer with strong business domain focus.\n\
			 - Resolve datasets; if schema is empty, call sql_register on candidates and proceed anyway with minimal staging models using {{ source('<pipeline>','<namespace>') }}.\n\
			 - Search DBT examples (search_dbt_examples) and adopt conventions from the top match.\n\
			 - Choose artifact type automatically (default DBT model). For project scaffolding, DO NOT build piece‑meal or ask per‑artifact approvals. Produce a consolidated batch of initial artifacts (staging/core/tests/docs) and save them in ONE call to approve_and_save_artifact_batch.\n\
			 - Validate with dbt_validate when available; if unavailable, proceed without blocking.\n\
			 - Ask the user only when confidence is very low (≤0.4) and only for concrete details; after any clarification, write a considered, sentient update from a fastidious custodian of data governance via catalog_note (preview if material).",
			question
		)
	} else {
		question.to_string()
	}
}


