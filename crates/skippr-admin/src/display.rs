use react_core::session::analysis::ThreadSummary;

use crate::accounting::{AccountProfile, Balance, DailyCost, LedgerEntry};

pub fn print_account_summary(profile: &AccountProfile, balance: &Balance, costs: &[DailyCost]) {
    println!();
    println!("  Account: {}", profile.tenant_id);
    println!("  Domain:  {}", profile.domain.as_deref().unwrap_or("-"));
    println!("  Plan:    {}", profile.plan);
    println!("  Status:  {}", profile.billing_status);
    println!("  Balance: ${:.2}", balance.balance);
    println!("  Version: {}", balance.version);
    println!();

    if !costs.is_empty() {
        println!("  Last {} days:", costs.len());
        println!("  {:<12} {:>10}", "Date", "Cost");
        println!("  {}", "-".repeat(24));
        let mut total = 0.0;
        for c in costs {
            println!("  {:<12} {:>10}", c.date, format!("${:.2}", c.cost));
            total += c.cost;
        }
        println!("  {}", "-".repeat(24));
        println!("  {:<12} {:>10}", "Total", format!("${:.2}", total));
    }
    println!();
}

pub fn print_ledger(entries: &[LedgerEntry]) {
    if entries.is_empty() {
        println!("  No ledger entries.");
        return;
    }
    println!();
    println!(
        "  {:<20} {:<8} {:>10} {:>12} Description",
        "Timestamp", "Type", "Amount", "After"
    );
    println!("  {}", "-".repeat(76));
    for e in entries {
        let sign = if e.entry_type == "credit" { "+" } else { "-" };
        println!(
            "  {:<20} {:<8} {:>10} {:>12} {}",
            &e.timestamp[..20.min(e.timestamp.len())],
            e.entry_type,
            format!("{}${:.4}", sign, e.amount.abs()),
            format!("${:.2}", e.balance_after),
            truncate(&e.description, 30),
        );
    }
    println!();
}

pub fn print_thread_list_with_dates(threads: &[(String, Option<chrono::DateTime<chrono::Utc>>)]) {
    if threads.is_empty() {
        println!("  No threads found.");
        return;
    }
    println!();
    println!("  Threads ({}):", threads.len());
    println!("  {:>4}  {:<38} {}", "#", "Thread ID", "Last Modified");
    println!("  {}", "-".repeat(68));
    for (i, (id, modified)) in threads.iter().enumerate() {
        let ts = modified
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "-".to_string());
        println!("  {:>4}  {:<38} {}", i + 1, id, ts);
    }
    println!();
}

pub fn print_thread_summary(thread_id: &str, summary: &ThreadSummary) {
    println!();
    println!("  Thread: {thread_id}");
    println!("  Steps:  {}", summary.total_steps);
    println!("  LLM calls: {}", summary.llm_calls);
    if let Some(dur) = summary.total_duration_ms {
        println!("  Duration: {:.1}s", dur as f64 / 1000.0);
    }
    if let Some(ref result) = summary.result {
        println!("  Result: {result}");
    }
    println!();

    if !summary.phases.is_empty() {
        println!("  Phases:");
        println!("  {:<20} {:>6} {:>10}", "Phase", "Steps", "Duration");
        println!("  {}", "-".repeat(40));
        for p in &summary.phases {
            let dur = p
                .duration_ms
                .map(|d| format!("{:.1}s", d as f64 / 1000.0))
                .unwrap_or_else(|| "-".into());
            println!("  {:<20} {:>6} {:>10}", p.name, p.steps, dur);
        }
        println!();
    }

    if !summary.tool_calls.is_empty() {
        println!("  Tool Usage:");
        println!("  {:<20} {:>6} {:>6} {:>6}", "Tool", "Calls", "OK", "Fail");
        println!("  {}", "-".repeat(42));
        for t in &summary.tool_calls {
            println!(
                "  {:<20} {:>6} {:>6} {:>6}",
                truncate(&t.name, 20),
                t.count,
                t.successes,
                t.failures,
            );
        }
        println!();
    }

    if !summary.issues.is_empty() {
        println!("  Issues:");
        for issue in &summary.issues {
            println!(
                "  [{:?}] {} (steps {}-{})",
                issue.kind, issue.description, issue.step_range.0, issue.step_range.1
            );
        }
        println!();
    }
}

pub fn print_tenant_list(tenants: &[(String, String)]) {
    if tenants.is_empty() {
        println!("  No tenants found.");
        return;
    }
    println!();
    println!("  Tenants ({}):", tenants.len());
    println!("  {:<38} {}", "Tenant ID", "Domain");
    println!("  {}", "-".repeat(64));
    for (id, domain) in tenants {
        let display = if domain.is_empty() { "-" } else { domain };
        println!("  {:<38} {}", id, display);
    }
    println!();
}

pub fn print_children(children: &[String], label: &str) {
    if children.is_empty() {
        println!("  (empty)");
        return;
    }
    println!();
    println!("  {} ({}):", label, children.len());
    for name in children {
        println!("    {name}");
    }
    println!();
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}
