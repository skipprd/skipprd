//! `skippr chat` — headless react threads with ask / plan / agent modes.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::headless_prep;

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
    pub pipeline: String,
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
    pub pipeline: String,
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Parser, Debug, Clone)]
pub struct ChatDocsSearchArgs {
    #[arg(long)]
    pub pipeline: String,
    #[arg(long)]
    pub query: String,
    #[arg(long, default_value_t = 8)]
    pub limit: usize,
    #[arg(long, default_value = "json")]
    pub output: String,
}

pub async fn run_chat(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    action: ChatAction,
) {
    match action {
        ChatAction::Send(args) => cmd_chat_send(log, explicit_config, args).await,
        ChatAction::Threads(args) => cmd_chat_threads(explicit_config, args).await,
        ChatAction::DocsSearch(args) => cmd_chat_docs_search(explicit_config, args).await,
    }
}

async fn cmd_chat_send(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: ChatSendArgs,
) {
    let ctx = match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    react_suite_data_engineer::metering::set_metering_run_id(&run_id);
    eprintln!("[skippr] chat run {run_id}");

    let metering = react_suite_data_engineer::metering::global_metering();
    let _ = metering
        .record_batch(&[
            react_suite_data_engineer::metering::UsageEvent::PipelineRun {
                project_id: args.pipeline.clone(),
            },
        ])
        .await;

    let (agent, prompt) = match args.mode {
        ChatModeCli::Ask => ("ask", args.message.clone()),
        ChatModeCli::Plan => (
            "ask",
            format!(
                "[plan mode — produce a data-engineering plan only; do not apply mutations]\n{}",
                args.message
            ),
        ),
        ChatModeCli::Agent => ("agent", args.message.clone()),
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
            headless_prompt: Some(prompt),
            stream_jsonl,
        },
    )
    .await;

    if stream_jsonl {
        let summary = serde_json::json!({
            "type": "ChatSummary",
            "pipeline": args.pipeline,
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
            "pipeline": args.pipeline,
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
    let ctx = match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
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
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
    }
}

async fn cmd_chat_docs_search(explicit_config: &Option<PathBuf>, args: ChatDocsSearchArgs) {
    let ctx = match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
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
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
    }
}
