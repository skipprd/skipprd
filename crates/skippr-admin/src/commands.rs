use std::sync::Arc;

use crate::keyspace_for_scope;
use react_core::provider_traits::NullSecretsProvider;
use react_core::session::analysis;
use react_core::session::ThreadStore;
use react_core::suite::SuiteCtx;
use react_core::thread_feedback::{ThreadFeedback, ThreadFeedbackStore, ThreadFeedbackVerdict};
use react_suite_debugger::SuiteDebugger;

use crate::accounting;
use crate::admin_scope;
use crate::code_index;
use crate::debug;
use crate::display;
use crate::nav::ShellState;

pub struct AppCtx {
    pub ddb_client: aws_sdk_dynamodb::Client,
    pub ddb_table: String,
    pub storage: Arc<dyn react_core::storage::StorageAdapter>,
    pub s3: Arc<react_module_storage_s3::S3StorageAdapter>,
    pub llm: react_core::llm::DynLlm,
    pub vector: Arc<dyn react_core::provider_traits::VectorStore>,
    pub suite_debugger: SuiteDebugger,
}

pub async fn dispatch(line: &str, state: &mut ShellState, app: &AppCtx) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let cmd = parts[0];
    let args = &parts[1..];
    let depth = state.depth();

    match cmd {
        "help" => print_help(depth),
        "quit" | "exit" => std::process::exit(0),
        "ls" => handle_ls(state, app).await,
        "cd" => handle_cd(args, state).await,
        ".." => {
            state.cd_up();
            state.refresh_children().await;
        }
        "account" if depth == 1 => {
            handle_account(state, app).await;
        }
        "ledger" if depth == 1 => {
            let limit = args
                .first()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(20);
            handle_ledger(state, app, limit).await;
        }
        "threads" if depth == 3 => {
            handle_threads(state, app).await;
        }
        "thread" if depth == 3 => {
            if let Some(id) = args.first() {
                handle_thread(state, app, id).await;
            } else {
                println!("  Usage: thread <thread_id>");
            }
        }
        "log" if depth == 3 => {
            if let Some(id) = args.first() {
                handle_log(state, app, id).await;
            } else {
                println!("  Usage: log <thread_id>");
            }
        }
        "debug" if depth == 3 => {
            if let Some(id) = args.first() {
                handle_debug(state, app, id).await;
            } else {
                println!("  Usage: debug <thread_id>");
            }
        }
        "feedback" if depth == 3 => handle_feedback(args, state, app).await,
        "fdebug" if depth == 3 => {
            if let Some(id) = args.first() {
                handle_feedback_debug(state, app, id).await;
            } else {
                println!("  Usage: fdebug <feedback_id>");
            }
        }
        _ => {
            println!("  Unknown command '{cmd}'. Type 'help' for available commands.");
        }
    }
}

fn print_help(depth: usize) {
    println!();
    println!("  Available commands:");
    println!("    ls             List items at current scope");
    println!("    cd [name]      Navigate into scope (fuzzy select if no name)");
    println!("    ..             Navigate up one level");
    if depth == 1 {
        println!("    account        Show tenant account summary");
        println!("    ledger [N]     Show last N ledger entries (default 20)");
    }
    if depth == 3 {
        println!("    threads        List threads in current project");
        println!("    thread <id>    Show thread summary");
        println!("    log <id>       Show raw thread run log");
        println!("    debug <id>     Start interactive LLM debug session");
        println!("    feedback       List unresolved feedback for this project");
        println!("    fdebug <id>    Debug the thread referenced by feedback");
    }
    println!("    help           Show this help");
    println!("    quit           Exit");
    println!();
}

async fn handle_ls(state: &mut ShellState, app: &AppCtx) {
    state.refresh_children().await;
    let children = state.children();

    if state.depth() == 0 {
        let mut tenant_rows: Vec<(String, String)> = Vec::new();
        for id in &children {
            let domain = accounting::get_profile(&app.ddb_client, &app.ddb_table, id)
                .await
                .ok()
                .and_then(|p| p.domain)
                .unwrap_or_default();
            tenant_rows.push((id.clone(), domain));
        }
        display::print_tenant_list(&tenant_rows);
    } else {
        let label = match state.depth() {
            1 => "Workspaces",
            2 => "Projects",
            _ => "Items",
        };
        display::print_children(&children, label);
    }
}

async fn handle_cd(args: &[&str], state: &mut ShellState) {
    let mut children = state.children();
    if children.is_empty() {
        state.refresh_children().await;
        children = state.children();
    }

    let name = if let Some(arg) = args.first() {
        arg.to_string()
    } else if children.is_empty() {
        println!("  No items to navigate into.");
        return;
    } else if children.len() == 1 {
        children[0].clone()
    } else {
        let selection = dialoguer::FuzzySelect::new()
            .with_prompt("Select")
            .items(&children)
            .default(0)
            .interact_opt();
        match selection {
            Ok(Some(idx)) => children[idx].clone(),
            _ => return,
        }
    };

    state.cd_into(name);
    state.refresh_children().await;
}

async fn handle_account(state: &ShellState, app: &AppCtx) {
    let tenant = match state.tenant() {
        Some(t) => t,
        None => {
            println!("  Navigate to a tenant first.");
            return;
        }
    };

    let profile = match accounting::get_profile(&app.ddb_client, &app.ddb_table, tenant).await {
        Ok(p) => p,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };
    let balance = match accounting::get_balance(&app.ddb_client, &app.ddb_table, tenant).await {
        Ok(b) => b,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };
    let costs =
        match accounting::get_daily_costs_est(&app.ddb_client, &app.ddb_table, tenant, 7).await {
            Ok(c) => c,
            Err(e) => {
                println!("  Error loading daily costs: {e}");
                Vec::new()
            }
        };

    display::print_account_summary(&profile, &balance, &costs);
}

async fn handle_ledger(state: &ShellState, app: &AppCtx, limit: i32) {
    let tenant = match state.tenant() {
        Some(t) => t,
        None => {
            println!("  Navigate to a tenant first.");
            return;
        }
    };

    match accounting::get_ledger(&app.ddb_client, &app.ddb_table, tenant, limit).await {
        Ok(entries) => display::print_ledger(&entries),
        Err(e) => println!("  Error: {e}"),
    }
}

async fn handle_threads(state: &ShellState, app: &AppCtx) {
    let scope = match state.request_scope() {
        Some(s) => s,
        None => {
            println!("  Navigate to a project first (tenant/workspace/project).");
            return;
        }
    };

    let keyspace = keyspace_for_scope();
    let prefix = keyspace.threads_prefix(&scope);
    let prefix = format!("{}/", prefix.trim_end_matches('/'));

    match app.s3.list_prefix_meta(&prefix).await {
        Ok(objects) => {
            let mut threads: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> = objects
                .into_iter()
                .filter_map(|obj| {
                    let name = obj.key.strip_prefix(&prefix)?.strip_suffix(".json")?;
                    if name.contains('/') || name.contains("__") || name.contains('.') {
                        return None;
                    }
                    Some((name.to_string(), obj.last_modified))
                })
                .collect();
            threads.sort_by(|a, b| b.1.cmp(&a.1));
            display::print_thread_list_with_dates(&threads);
        }
        Err(e) => println!("  Error: {e}"),
    }
}

async fn handle_thread(state: &ShellState, app: &AppCtx, thread_id: &str) {
    let scope = match state.request_scope() {
        Some(s) => s,
        None => {
            println!("  Navigate to a project first.");
            return;
        }
    };

    let keyspace = keyspace_for_scope();
    let store = ThreadStore::new(app.storage.clone(), scope, keyspace);
    match store.get(thread_id).await {
        Ok(log) => {
            let summary = analysis::summarize(&log);
            display::print_thread_summary(thread_id, &summary);
        }
        Err(e) => println!("  Error: {e}"),
    }
}

async fn handle_log(state: &ShellState, app: &AppCtx, thread_id: &str) {
    let scope = match state.request_scope() {
        Some(s) => s,
        None => {
            println!("  Navigate to a project first.");
            return;
        }
    };

    let keyspace = keyspace_for_scope();
    let key = match keyspace.thread_log_key(&scope, thread_id) {
        Ok(k) => k,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };

    match app.storage.get_bytes(&key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            println!("{text}");
        }
        Err(e) => println!("  Error reading log: {e}"),
    }
}

async fn handle_debug(state: &ShellState, app: &AppCtx, thread_id: &str) {
    let target_scope = match state.request_scope() {
        Some(s) => s,
        None => {
            println!("  Navigate to a project first.");
            return;
        }
    };
    let admin_scope = match admin_scope::derive_admin_scope(&target_scope) {
        Ok(scope) => scope,
        Err(e) => {
            println!("  Failed to derive admin scope: {e}");
            return;
        }
    };

    let keyspace = keyspace_for_scope();
    let mut ctx = SuiteCtx::new(
        app.storage.clone(),
        Arc::new(NullSecretsProvider),
        app.llm.clone(),
        admin_scope.clone(),
        keyspace,
    );
    ctx.set_vector(Some(app.vector.clone()));
    debug::wire_debug_capabilities(&mut ctx, target_scope.clone());

    println!("  Preparing debug session for thread {thread_id}...");
    println!("  Checking admin repo index (this can take a while on the first run)...");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    match code_index::ensure_repo_indexed_with_progress(
        &ctx,
        &code_index::DEFAULT_REPO_ROOTS
            .iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>(),
        |msg| {
            println!("  {msg}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        },
    )
    .await
    {
        Ok(status) => {
            if status.reused_existing_index {
                println!(
                    "  Reusing admin repo index in {} files / {} chunks.",
                    status.file_count, status.chunk_count
                );
            } else {
                println!(
                    "  Refreshed admin repo index from {} files / {} chunks.",
                    status.file_count, status.chunk_count
                );
            }
            for warning in status.warnings {
                println!("  Warning: {warning}");
            }
        }
        Err(e) => {
            println!("  Warning: admin repo indexing skipped: {e}");
        }
    }

    println!(
        "  Starting debug session for thread {thread_id} (admin scope: {}/{}/{})...",
        admin_scope.tenant, admin_scope.workspace, admin_scope.project_id
    );
    println!("  Type '/quit' to exit.\n");

    if let Err(e) = debug::run_debug(thread_id, &app.suite_debugger, &ctx).await {
        println!("  Debug error: {e}");
    }
}

async fn handle_feedback(args: &[&str], state: &ShellState, app: &AppCtx) {
    match args {
        [] => handle_feedback_list(state, app, false).await,
        ["all"] => handle_feedback_list(state, app, true).await,
        ["resolve", feedback_id] => handle_feedback_resolve(state, app, feedback_id).await,
        ["debug", feedback_id] => handle_feedback_debug(state, app, feedback_id).await,
        _ => {
            println!("  Usage: feedback");
            println!("         feedback all");
            println!("         feedback resolve <feedback_id>");
            println!("         feedback debug <feedback_id>");
        }
    }
}

async fn handle_feedback_list(state: &ShellState, app: &AppCtx, include_resolved: bool) {
    let store = match feedback_store(state, app) {
        Ok(store) => store,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };
    let feedback = match store.list_all().await {
        Ok(items) => filter_feedback(items, include_resolved),
        Err(e) => {
            println!("  Error loading feedback: {e}");
            return;
        }
    };
    if feedback.is_empty() {
        if include_resolved {
            println!("  No feedback entries found for this project.");
        } else {
            println!("  No unresolved feedback entries found for this project.");
        }
        return;
    }

    println!();
    for item in feedback {
        let verdict = match item.verdict {
            ThreadFeedbackVerdict::Good => "good",
            ThreadFeedbackVerdict::Bad => "bad",
        };
        let status = if item.resolved { "resolved" } else { "open" };
        println!(
            "  {} [{} / {}] thread {}",
            item.feedback_id, verdict, status, item.thread_id
        );
        println!("    created: {}", item.created_at);
        if let Some(resolved_at) = &item.resolved_at {
            println!("    resolved: {}", resolved_at);
        }
        for line in item.comment.lines() {
            println!("    {line}");
        }
        println!();
    }
}

async fn handle_feedback_resolve(state: &ShellState, app: &AppCtx, feedback_id: &str) {
    let store = match feedback_store(state, app) {
        Ok(store) => store,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };
    match store.mark_resolved(feedback_id).await {
        Ok(Some(item)) => println!(
            "  Marked feedback {} as resolved for thread {}.",
            item.feedback_id, item.thread_id
        ),
        Ok(None) => println!("  Feedback {feedback_id} was not found in this project."),
        Err(e) => println!("  Error resolving feedback: {e}"),
    }
}

async fn handle_feedback_debug(state: &ShellState, app: &AppCtx, feedback_id: &str) {
    let store = match feedback_store(state, app) {
        Ok(store) => store,
        Err(e) => {
            println!("  Error: {e}");
            return;
        }
    };
    match store.get_by_id(feedback_id).await {
        Ok(Some(item)) => {
            println!(
                "  Debugging thread {} from feedback {}...",
                item.thread_id, item.feedback_id
            );
            handle_debug(state, app, &item.thread_id).await;
        }
        Ok(None) => println!("  Feedback {feedback_id} was not found in this project."),
        Err(e) => println!("  Error loading feedback: {e}"),
    }
}

fn feedback_store(state: &ShellState, app: &AppCtx) -> Result<ThreadFeedbackStore, String> {
    let scope = state
        .request_scope()
        .ok_or_else(|| "Navigate to a project first.".to_string())?;
    Ok(ThreadFeedbackStore::new(
        app.storage.clone(),
        scope,
        keyspace_for_scope(),
    ))
}

fn filter_feedback(feedback: Vec<ThreadFeedback>, include_resolved: bool) -> Vec<ThreadFeedback> {
    if include_resolved {
        feedback
    } else {
        feedback.into_iter().filter(|item| !item.resolved).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_feedback_hides_resolved_entries_by_default() {
        let items = vec![
            ThreadFeedback {
                feedback_id: "a".to_string(),
                thread_id: "thread-1".to_string(),
                verdict: ThreadFeedbackVerdict::Bad,
                comment: "needs work".to_string(),
                created_at: "2026-04-03T10:00:00Z".to_string(),
                resolved: false,
                resolved_at: None,
            },
            ThreadFeedback {
                feedback_id: "b".to_string(),
                thread_id: "thread-2".to_string(),
                verdict: ThreadFeedbackVerdict::Good,
                comment: "looks right".to_string(),
                created_at: "2026-04-03T11:00:00Z".to_string(),
                resolved: true,
                resolved_at: Some("2026-04-03T12:00:00Z".to_string()),
            },
        ];

        let unresolved = filter_feedback(items.clone(), false);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].feedback_id, "a");

        let all = filter_feedback(items, true);
        assert_eq!(all.len(), 2);
    }
}
