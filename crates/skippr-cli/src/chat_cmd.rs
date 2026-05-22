//! `skippr chat` — headless react threads with ask / plan / agent modes.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::headless_prep;
use react_suite_data_engineer::PipelineName;

#[derive(Subcommand, Debug, Clone)]
pub enum ChatAction {
    /// Send a message on a new or existing thread.
    Send(ChatSendArgs),
    /// List recent thread ids for the pipeline (storage-backed).
    Threads(ChatThreadsArgs),
    /// Semantic search over Skippr public documentation (requires knowledge STS on credentials).
    DocsSearch(ChatDocsSearchArgs),
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatModeCli {
    Ask,
    Plan,
    Agent,
}

#[derive(Parser, Debug, Clone)]
pub struct ChatSendArgs {
    #[arg(long)]
    pub pipeline: Option<PipelineName>,
    #[arg(long, value_enum)]
    pub mode: ChatModeCli,
    #[arg(long)]
    pub message: String,
    /// Resume this thread id (UUID). When omitted, a new thread is created.
    #[arg(long)]
    pub thread: Option<String>,
    /// Output: `text` (human), `json` (single summary object), or `jsonl` (streamed events + trailing ChatSummary).
    #[arg(long, default_value = "text")]
    pub output: String,
}

#[derive(Parser, Debug, Clone)]
pub struct ChatThreadsArgs {
    #[arg(long)]
    pub pipeline: PipelineName,
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Parser, Debug, Clone)]
pub struct ChatDocsSearchArgs {
    #[arg(long)]
    pub pipeline: PipelineName,
    #[arg(long)]
    pub query: String,
    #[arg(long, default_value_t = 8)]
    pub limit: usize,
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Debug, Deserialize)]
struct ChatContextEnvelope {
    user: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    execution_surface: Option<String>,
}

fn parse_chat_message(raw: &str) -> (String, Option<serde_json::Value>, Option<String>) {
    let trimmed = raw.trim();
    let Ok(envelope) = serde_json::from_str::<ChatContextEnvelope>(trimmed) else {
        return (raw.to_string(), None, None);
    };
    if envelope.user.trim().is_empty() {
        return (raw.to_string(), None, None);
    }
    let context = if envelope.context.is_null() {
        None
    } else {
        Some(envelope.context)
    };
    (envelope.user, context, envelope.execution_surface)
}

fn render_structured_chat_prompt(user: &str, context: Option<&serde_json::Value>) -> String {
    let Some(context) = context else {
        return user.to_string();
    };
    format!(
        "User request:\n{}\n\nStructured context JSON:\n{}\n\nUse this context only if it is relevant. For explicit local or attached file edits, read the named file directly and patch it; do not use vector retrieval first. Use vector retrieval only for broad documentation or artifact lookup when no concrete local file is named.",
        user,
        serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string())
    )
}

fn render_workspace_chat_prompt(user_prompt: &str) -> String {
    format!(
        "Workspace-scoped ask mode:\n\
         - The user did not select a single pipeline.\n\
         - Use skippr_cli(command:\"config\", action:\"show\") first to inspect configured pipelines and connections.\n\
         - For data questions, choose one or more relevant configured pipelines from the user's intent and config metadata.\n\
         - Use skippr_cli(command:\"lineage\", action:\"graph\", pipeline:<pipeline>) and skippr_cli(command:\"query\", pipeline:<pipeline>, sql:<read-only SELECT/WITH>) as needed.\n\
         - Every data-bearing tool call must include an explicit pipeline.\n\
         - Cite which pipeline(s) and dataset(s) support the answer. Ask a clarification only when the configured context is insufficient or materially ambiguous.\n\n\
         {user_prompt}"
    )
}

pub async fn run_chat(log: Option<String>, explicit_config: &Option<PathBuf>, action: ChatAction) {
    match action {
        ChatAction::Send(args) => cmd_chat_send(log, explicit_config, args).await,
        ChatAction::Threads(args) => cmd_chat_threads(explicit_config, args).await,
        ChatAction::DocsSearch(args) => cmd_chat_docs_search(explicit_config, args).await,
    }
}

async fn cmd_chat_send(log: Option<String>, explicit_config: &Option<PathBuf>, args: ChatSendArgs) {
    if let Some(path) = explicit_config.as_ref() {
        std::env::set_var("SKIPPR_CONFIG_FILE", path);
    }
    let workspace_scoped = args.pipeline.is_none();
    let target = args
        .pipeline
        .clone()
        .map(headless_prep::ChatTarget::Pipeline)
        .unwrap_or(headless_prep::ChatTarget::GenericIdeBootstrap);
    let ctx = match headless_prep::authenticate_headless_for_chat(explicit_config, target).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let project_id = args
        .pipeline
        .as_ref()
        .map(|pipeline| pipeline.as_str())
        .unwrap_or("workspace-chat");

    let run_id = uuid::Uuid::new_v4().to_string();
    react_suite_data_engineer::metering::set_metering_run_id(&run_id);
    eprintln!("[skippr] chat run {run_id}");

    let metering = react_suite_data_engineer::metering::global_metering();
    let _ = metering
        .record_batch(&[
            react_suite_data_engineer::metering::UsageEvent::PipelineRun {
                project_id: project_id.to_string(),
            },
        ])
        .await;

    let (user_message, structured_context, execution_surface) = parse_chat_message(&args.message);
    if execution_surface.as_deref() == Some("ide_chat") {
        std::env::set_var("SKIPPR_EXECUTION_SURFACE", "ide_chat");
    } else {
        std::env::remove_var("SKIPPR_EXECUTION_SURFACE");
    }
    if workspace_scoped {
        std::env::set_var("SKIPPR_WORKSPACE_SCOPED_CHAT", "1");
    } else {
        std::env::remove_var("SKIPPR_WORKSPACE_SCOPED_CHAT");
    }
    let rendered_prompt = render_structured_chat_prompt(&user_message, structured_context.as_ref());
    let _rendered_prompt = if workspace_scoped {
        render_workspace_chat_prompt(&rendered_prompt)
    } else {
        rendered_prompt
    };

    let agent = match args.mode {
        ChatModeCli::Ask => "ask",
        ChatModeCli::Plan => "ask",
        ChatModeCli::Agent => "agent",
    };

    let mode_str = match args.mode {
        ChatModeCli::Ask => "ask",
        ChatModeCli::Plan => "plan",
        ChatModeCli::Agent => "agent",
    };

    let thread_id = args.thread.clone();
    if let Some(ref tid) = thread_id {
        react_suite_data_engineer::metering::set_metering_thread_id(tid);
        eprintln!("[skippr] chat resuming thread {tid}");
    } else {
        eprintln!("[skippr] chat starting new thread");
    }

    let stream_jsonl = crate::is_jsonl_output(&args.output);
    let headless = crate::react_host::run_headless_detailed(
        ctx.resolved,
        react::run_engine::HeadlessRunOpts {
            log_level: log,
            verbose_debug: false,
            terminal: false,
            thread_id,
            suite_id: Some("data_engineer".to_string()),
            agent: agent.to_string(),
            skip_logging_init: false,
        },
    )
    .await;

    if stream_jsonl {
        let summary = serde_json::json!({
            "type": "ChatSummary",
            "pipeline": args.pipeline.as_ref().map(|pipeline| pipeline.as_str()),
            "workspace_scoped": workspace_scoped,
            "mode": mode_str,
            "thread_id": headless.thread_id,
            "ok": headless.exit_code == 0,
            "exit_code": headless.exit_code,
            "bootstrap_error": headless.bootstrap_error,
            "failure_summary": headless.failure_summary,
        });
        println!("{}", serde_json::to_string(&summary).unwrap_or_default());
    }

    if crate::is_json_output(&args.output) {
        let body = serde_json::json!({
            "ok": headless.exit_code == 0,
            "exit_code": headless.exit_code,
            "bootstrap_error": headless.bootstrap_error,
            "failure_summary": headless.failure_summary,
            "thread_id": headless.thread_id,
            "pipeline": args.pipeline.as_ref().map(|pipeline| pipeline.as_str()),
            "workspace_scoped": workspace_scoped,
            "mode": mode_str,
        });
        crate::print_json(&body);
    } else if !stream_jsonl {
        if let Some(err) = headless.bootstrap_error.as_deref() {
            eprintln!("[skippr] chat bootstrap error: {err}");
        }
        eprintln!(
            "[skippr] chat finished with exit code {}",
            headless.exit_code
        );
    }

    if headless.exit_code != 0 {
        std::process::exit(headless.exit_code);
    }
}

async fn cmd_chat_threads(explicit_config: &Option<PathBuf>, args: ChatThreadsArgs) {
    let ctx =
        match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[skippr] ERROR: {e}");
                std::process::exit(1);
            }
        };

    let list = match crate::list_threads_for_resolved_config(&ctx.resolved).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let rows: Vec<serde_json::Value> = list
        .into_iter()
        .map(|(thread_id, ts)| {
            serde_json::json!({
                "thread_id": thread_id,
                "last_modified": ts.to_rfc3339(),
            })
        })
        .collect();

    let body = serde_json::json!({
        "ok": true,
        "pipeline": args.pipeline,
        "threads": rows,
    });

    if crate::is_json_output(&args.output) || args.output == "json" {
        crate::print_json(&body);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
    }
}

async fn cmd_chat_docs_search(explicit_config: &Option<PathBuf>, args: ChatDocsSearchArgs) {
    let ctx =
        match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[skippr] ERROR: {e}");
                std::process::exit(1);
            }
        };

    let srv = match ctx.client.get_credentials().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: credentials fetch failed: {e}");
            std::process::exit(1);
        }
    };

    let hits = match crate::public_docs_search::search_public_skippr_docs(
        &ctx.resolved,
        &srv,
        &args.query,
        args.limit.max(1).min(50),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let body = serde_json::json!({
        "ok": true,
        "query": args.query,
        "hits": hits,
    });

    if crate::is_json_output(&args.output) || args.output == "json" {
        crate::print_json(&body);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
    }
}
