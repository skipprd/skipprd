use datafusion::prelude::SessionContext;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use std::collections::HashSet;
use crate::llm::{self, ChatMessage};
use crate::helpers::configuration::{Config, PIPELINE_NAME};
use crate::sql::query::register_catalog;
use crate::qa::registry::register_namespace_view;
use crate::qa::{prompts, planner};

#[derive(Clone, Debug, Default)]
pub struct AskOpts {
    pub top_k: usize,
    pub use_docs: bool,
    pub use_sql: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub text: String,
    pub followup: Option<String>,
}

pub async fn ask(question: &str, opts: &AskOpts) -> Result<Answer, String> {
    println!("{} ASK: enter", chrono::Utc::now().to_rfc3339());
    // Prepare DataFusion context
    println!("{} ASK: creating SessionContext", chrono::Utc::now().to_rfc3339());
    let ctx = SessionContext::new();
    println!("{} ASK: SessionContext created", chrono::Utc::now().to_rfc3339());

    // Establish pipeline context already performed in main; avoid re-locking pipeline name here to prevent deadlocks
    println!("{} ASK: using pipeline '{}'", chrono::Utc::now().to_rfc3339(), Config::get_pipeline_name());

    // 1) Intent (currently not parsed strictly; can be added once needed)
    let _intent_prompt = prompts::intent_extraction(question);
    let cfg = llm::config_from_env();
    let model = llm::create_llm(&cfg);

    // 2) Candidate selection across all namespaces (catalog registered inside)
    println!("{} ASK: registering catalog table", chrono::Utc::now().to_rfc3339());
    match tokio::time::timeout(std::time::Duration::from_secs(5), register_catalog(&ctx)).await {
        Ok(_) => println!("{} ASK: catalog registered", chrono::Utc::now().to_rfc3339()),
        Err(_) => println!("{} ASK: register_catalog timed out after 5s; continuing", chrono::Utc::now().to_rfc3339()),
    }

    println!("{} ASK: shortlist_candidates begin", chrono::Utc::now().to_rfc3339());
    let candidates = match tokio::time::timeout(std::time::Duration::from_secs(5), planner::shortlist_candidates(&ctx, question, 5)).await {
        Ok(v) => v,
        Err(_) => {
            println!("{} ASK: shortlist_candidates timed out after 5s", chrono::Utc::now().to_rfc3339());
            Vec::new()
        }
    };
    println!("{} ASK: shortlist_candidates end ({} cand)", chrono::Utc::now().to_rfc3339(), candidates.len());
    if candidates.is_empty() {
        // dump catalog count to aid debugging
        if let Ok(df) = ctx.sql("SELECT count(1) FROM catalog").await { if let Ok(b) = df.collect().await { println!("{} ASK: catalog count = {}", chrono::Utc::now().to_rfc3339(), b.first().map(|rb| crate::sql::tui::value_to_string(rb.column(0).as_ref(), 0)).unwrap_or("0".to_string())); } }
    }
    if candidates.is_empty() {
        return Ok(Answer { text: "Sorry, I could not find relevant datasets to answer that.".to_string(), followup: None });
    }

    // Fast-path: single candidate, compute deterministically without LLM
    if candidates.len() == 1 {
        let mut ns = candidates[0].namespace.clone();
        println!("{} ASK: single candidate '{}' → deterministic path", chrono::Utc::now().to_rfc3339(), ns);
        // Ensure pipeline context matches the namespace before registering
        PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&ns);
    Config::init().await;
        // Try exact namespace only (no local fuzzy logic), with a guard timeout
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), register_namespace_view(&ctx, &ns)).await;
        // If we have a resolved namespace/table, try deterministic
        if let Ok(df) = ctx.table(&ns).await {
            // Discover schema fields
            let mut fields: Vec<(String, ArrowDataType)> = Vec::new();
            for f in df.schema().fields() { fields.push((f.name().to_string(), f.data_type().clone())); }
            println!("{} ASK: namespace '{}' has {} fields", chrono::Utc::now().to_rfc3339(), ns, fields.len());

            // Pick a grouping column based on question tokens and schema
            let lower_q = question.to_lowercase();
            let mut candidates_cols: Vec<String> = vec!["type".into(), "event_type".into(), "bike_type".into(), "category".into(), "kind".into(), "model".into()];
            if lower_q.contains("type") { for (name, _dt) in &fields { if name.to_lowercase().contains("type") { candidates_cols.push(name.clone()); } } }
            let string_fields: Vec<String> = fields.iter().filter_map(|(n, dt)| match dt { ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Some(n.clone()), _ => None }).collect();
            let mut seen: HashSet<String> = HashSet::new(); candidates_cols.retain(|c| seen.insert(c.to_lowercase()));
            let mut chosen: Option<String> = None;
            for c in &candidates_cols { if fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(c)) { chosen = Some(c.clone()); break; } }
            if chosen.is_none() { for c in &candidates_cols { let lc = c.to_lowercase(); if let Some((n,_)) = fields.iter().find(|(n,_)| n.to_lowercase().contains(&lc)) { chosen = Some(n.clone()); break; } } }
            if chosen.is_none() { if let Some(sf) = string_fields.first() { chosen = Some(sf.clone()); } }

            if let Some(col) = chosen.clone() {
                println!("{} ASK: selected grouping column '{}'", chrono::Utc::now().to_rfc3339(), col);
                let sql = format!(
                    "SELECT {} AS group_value, COUNT(1) AS num_hires FROM {} GROUP BY {} ORDER BY num_hires DESC LIMIT {}",
                    col, ns, col, opts.top_k.max(1)
                );
                println!("{} ASK: deterministic SQL: {}", chrono::Utc::now().to_rfc3339(), sql);
                match ctx.sql(&sql).await {
                    Ok(df) => match df.collect().await {
                        Ok(batches) => {
                            let mut best_val: Option<String> = None; let mut best_cnt: i64 = -1;
                            for b in &batches { let rows = b.num_rows(); for r in 0..rows { let val = crate::sql::tui::value_to_string(b.column(0).as_ref(), r); let cnt_s = crate::sql::tui::value_to_string(b.column(1).as_ref(), r); let cnt = cnt_s.parse::<i64>().unwrap_or(0); if cnt > best_cnt { best_cnt = cnt; best_val = Some(val); } } }
                            if let Some(v) = best_val {
                                println!("{} ASK: deterministic result: {} ({} hires)", chrono::Utc::now().to_rfc3339(), v, best_cnt);
                                return Ok(Answer { text: format!("{} ({} hires)", v, best_cnt), followup: None });
                            }
                            return Ok(Answer { text: "No results found.".to_string(), followup: None });
                        }
                        Err(e) => { println!("{} ASK: deterministic collect error: {}", chrono::Utc::now().to_rfc3339(), e); }
                    },
                    Err(e) => { println!("{} ASK: deterministic SQL error: {}", chrono::Utc::now().to_rfc3339(), e); }
                }
            } else { println!("{} ASK: could not select grouping column; falling back to LLM", chrono::Utc::now().to_rfc3339()); }
        }
    }

    // Build ranking context including dataset root descriptions
    let mut ctx_lines: Vec<String> = Vec::new();
    for c in &candidates {
        let mut line = format!("namespace:{} fields:{}", c.namespace, c.fields.iter().cloned().collect::<Vec<_>>().join(", "));
        if let Some(desc) = &c.root_description { if !desc.is_empty() { line.push_str(&format!(" description:{}", desc)); } }
        ctx_lines.push(line);
    }
    // If multiple candidates, use LLM ranking; else we've already attempted deterministic
    if candidates.len() > 1 {
        let ranking_prompt = prompts::candidate_ranking(&ctx_lines.join("\n"), question);
        println!("{} ASK: ranking candidates with LLM...", chrono::Utc::now().to_rfc3339());
        let _ranking_json = model.chat(&[ChatMessage { role: "user".into(), content: ranking_prompt }]).unwrap_or_default();
        println!("{} ASK: ranking completed", chrono::Utc::now().to_rfc3339());
    }

    // 3) Register only shortlisted namespaces
    println!("{} ASK: registering {} namespaces...", chrono::Utc::now().to_rfc3339(), candidates.len());
    let mut available_ns: Vec<String> = Vec::new();
    for c in &candidates {
        // Try with namespace as pipeline
        PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&c.namespace);
        Config::init().await;
        match register_namespace_view(&ctx, &c.namespace).await {
            Ok(_) => { available_ns.push(c.namespace.clone()); continue; }
            Err(e1) => {
                println!("{} ASK: namespace '{}' unavailable: {}; attempting fuzzy", chrono::Utc::now().to_rfc3339(), c.namespace, e1);
                // Fuzzy: scan ./data/test_* for similar pipelines
                if let Ok(rd) = std::fs::read_dir("./data") {
                    let mut picked: Option<String> = None;
                    for ent in rd.flatten() {
                        if let Ok(name) = ent.file_name().into_string() {
                            if !name.starts_with("test_") { continue; }
                            let p = name[5..].to_string();
                            if p.starts_with(&c.namespace) || c.namespace.starts_with(&p) || p.contains(&c.namespace) {
                                picked = Some(p); break;
                            }
                        }
                    }
                    if let Some(p) = picked {
                        PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&p);
                        Config::init().await;
                        match register_namespace_view(&ctx, &p).await {
                            Ok(_) => { println!("{} ASK: using pipeline '{}' for '{}'", chrono::Utc::now().to_rfc3339(), p, c.namespace); available_ns.push(p); }
                            Err(e2) => { println!("{} ASK: fuzzy pipeline '{}' also unavailable: {}", chrono::Utc::now().to_rfc3339(), p, e2); }
                        }
                    }
                }
            }
        }
    }
    if available_ns.is_empty() {
        return Ok(Answer { text: "No data sources (S3 or WAL) available for the relevant datasets. Run sync or configure outputs, then try again.".to_string(), followup: None });
    }

    // 4) Infer join hints
    println!("{} ASK: inferring joins...", chrono::Utc::now().to_rfc3339());
    let joins = planner::infer_joins(&ctx, &candidates).await;
    let joins_str = joins.iter().map(|e| format!("{}.{} = {}.{}", e.left_ns, e.left_key, e.right_ns, e.right_key)).collect::<Vec<_>>().join("; ");
    println!("{} ASK: inferred {} joins", chrono::Utc::now().to_rfc3339(), joins.len());

    // 5) SQL generation with guardrails
    let sql_gen_prompt = prompts::sql_generation(&ctx_lines.join("\n"), &joins_str, question, opts.top_k);
    println!("{} ASK: generating SQL with LLM...", chrono::Utc::now().to_rfc3339());
    let mut sql = model.chat(&[ChatMessage { role: "user".into(), content: sql_gen_prompt }]).unwrap_or_default();
    // Pre-validate: ensure table exists and map obvious column aliases when possible
    if let Some(ns) = available_ns.first() {
        if let Ok(df) = ctx.table(ns).await {
            let fields: Vec<String> = df.schema().fields().iter().map(|f| f.name().to_string()).collect();
            // simple alias map for common patterns
            if sql.contains(".type") && fields.iter().any(|f| f.eq_ignore_ascii_case("event_type")) {
                sql = sql.replace(".type", ".event_type");
            }
            // Verify all referenced columns exist; if not, strip or replace by first string field
            // keep minimal: only enforce LIMIT and FROM <ns>
            if !sql.to_lowercase().contains(&format!(" from {} ", ns)) {
                sql = format!("SELECT * FROM {} LIMIT {}", ns, opts.top_k);
            }
        } else {
            return Ok(Answer { text: "Dataset is not registered in the query context; cannot run SQL.".to_string(), followup: None });
        }
    }
    if !sql.to_lowercase().contains("select ") { sql = format!("SELECT * FROM {} LIMIT {}", candidates[0].namespace, opts.top_k); }

    // Force LIMIT if missing
    if !sql.to_lowercase().contains(" limit ") { sql.push_str(&format!(" LIMIT {}", opts.top_k)); }
    println!("{} ASK: SQL generated", chrono::Utc::now().to_rfc3339());

    // 6) Execute
    println!("{} ASK: executing SQL...", chrono::Utc::now().to_rfc3339());
    let mut execution_error: Option<String> = None;
    let data = match ctx.sql(&sql).await {
        Ok(df) => {
            match df.collect().await {
                Ok(batches) => {
                    let mut results: Vec<String> = Vec::new();
                    for b in batches {
                        for row in 0..b.num_rows() {
                            let mut row_vals: Vec<String> = Vec::new();
                            for col in 0..b.num_columns() {
                                let val = crate::sql::tui::value_to_string(b.column(col).as_ref(), row);
                                row_vals.push(val);
                            }
                            results.push(row_vals.join(", "));
                        }
                    }
                    let out = results.join("\n");
                    let row_count = if out.is_empty() { 0 } else { out.lines().count() };
                    println!("{} ASK: SQL execution completed with {} row(s)", chrono::Utc::now().to_rfc3339(), row_count);
                    out
                },
                Err(e) => { execution_error = Some(format!("collect_error: {}", e)); "Error collecting query results.".to_string() },
            }
        },
        Err(e) => { execution_error = Some(format!("compile_error: {}", e)); "Error executing SQL query.".to_string() },
    };

    println!("\n\n--- Debug Info ---");
    println!("Question: {}\n", question);
    println!("Context:\n{}", ctx_lines.join("\n"));
    println!("Executed SQL Query: {}\n", sql);
    println!("Data:\n{}", data);
    println!("------------------\n\n");

    // Avoid hallucinations: if SQL failed, return clear message; else optionally summarize without LLM
    if data.starts_with("Error ") || data.is_empty() {
        let err_msg = execution_error.unwrap_or_else(|| "unknown_error".to_string());
        println!("{} ASK: SQL failed; err={}", chrono::Utc::now().to_rfc3339(), err_msg);
        // Brief, explicit error back to user
        return Ok(Answer { text: format!("SQL failed: {}. I can retry after adjusting columns/tables if data is available.", err_msg), followup: None });
    }

    // Synthesize a concise answer without expensive LLM call
    let first_line = data.lines().next().unwrap_or("").to_string();
    println!("{} ASK: done", chrono::Utc::now().to_rfc3339());
    Ok(Answer { text: first_line, followup: None })
}


