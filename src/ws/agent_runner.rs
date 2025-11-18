use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;

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

pub fn build_registry(agent: &str, ctx: &SessionContext) -> ToolRegistry {
	use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool, ask_user::AskUserTool};
	use crate::qa::tools::{approve_save::ApproveAndSaveArtifactTool, artifacts::ArtifactsTool};
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
			 - Always call ask_user to request approval or edits before saving.\n\
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


