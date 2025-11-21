use datafusion::prelude::SessionContext;
use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool};
use std::cmp::Ordering;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub async fn run(question: &str, pipeline: &str, namespace: Option<&str>) -> Result<String, String> {
    let ctx = SessionContext::new();
    // Auto-register all pipelines/namespaces so queries don't require a specific pipeline flag
    {
        let pipelines = crate::sql::registry::list_pipelines().await;
        for p in pipelines {
            let mut namespaces = crate::sql::registry::list_namespaces(&p).await;
            namespaces.sort();
            for ns in namespaces {
                let _ = crate::sql::tables::register_namespace_view(&ctx, &p, &ns).await;
            }
            // Also register deadletters (best-effort)
            let _ = crate::sql::tables::register_deadletters(&ctx, &p).await;
        }
    }
    // Pre-fetch embedding candidates across all pipelines (dataset scope) and print as plain text
    let mut embeds_block: String = String::new();
    let mut company_info_block: String = String::new();
    let mut qvec_opt: Option<Vec<f32>> = None;
    {
        let cfg = crate::llm::config_from_env();
        let model = crate::llm::create_llm(&cfg);
        let vecs = model.embed(&[question.to_string()]).map_err(|e| e.to_string()).unwrap_or_default();
        if let Some(qvec) = vecs.get(0) {
            qvec_opt = Some(qvec.clone());
            let mut all_hits: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
            let pipelines = crate::sql::registry::list_pipelines().await;
            for p in pipelines {
                let store = crate::qa::vector::lance_store::LanceDbStore::new(&p);
                if let Ok(mut v) = store.query(qvec, 30, Some("dataset")).await { all_hits.append(&mut v); }
            }
            if !all_hits.is_empty() {
                all_hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal));
                let mut seen = std::collections::HashSet::<String>::new();
                let mut dedup: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
                for h in all_hits {
                    if seen.insert(h.item.id.clone()) { dedup.push(h); }
                    if dedup.len() >= 15 { break; }
                }
                println!("Embeddings: candidate datasets (top {}):", dedup.len());
                embeds_block.push_str("EmbeddingCandidates:\n");
                for h in dedup.iter() {
                    let ns = &h.item.namespace;
                    // Derive pipeline from id "<kind>:<pipeline>:<namespace>[:field]"
                    let parts: Vec<&str> = h.item.id.split(':').collect();
                    let pipeline_fqn = if parts.len() >= 3 { parts[1].to_string() } else { pipeline.to_string() };
                    let fqn = format!("{}.{}", pipeline_fqn, ns);
                    // Safe UTF-8 truncate at char boundary
                    let txt = {
                        let s = &h.item.text;
                        if s.len() <= 160 { s.to_string() } else {
                            match s.char_indices().take_while(|(i, _)| *i <= 160).last() {
                                Some((i, _)) => s[..i].to_string(),
                                None => String::new(),
                            }
                        }
                    };
                    println!(" - dataset={} score={:.4} text=\"{}\"", fqn, h.score, txt);
                    embeds_block.push_str(&format!("- {} (score {:.4}): {}\n", fqn, h.score, txt));
                }
            } else {
                println!("Embeddings: no candidate datasets found");
            }
        }
        // Company information block (pre-agent): try to fetch doc embeddings; if none, ask user and upsert now.
        if let Some(qv) = qvec_opt.as_ref() {
            let store = crate::qa::vector::lance_store::LanceDbStore::new(pipeline);
            if let Ok(mut hits) = store.query(qv, 10, Some("doc")).await {
                hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal));
                let mut seen = std::collections::HashSet::<String>::new();
                let mut texts: Vec<String> = Vec::new();
                for h in hits {
                    if seen.insert(h.item.id.clone()) {
                        let t = h.item.text.trim();
                        if !t.is_empty() { texts.push(t.to_string()); }
                    }
                    if texts.len() >= 3 { break; }
                }
                if !texts.is_empty() {
                    company_info_block.push_str("CompanyInfo:\n");
                    for t in texts { company_info_block.push_str(&format!("- {}\n", t)); }
                } else {
                    println!("Please tell me a little about your business (1-3 sentences):");
                    let mut buf = String::new();
                    let _ = std::io::stdin().read_line(&mut buf);
                    let text = buf.trim().to_string();
                    if !text.is_empty() {
                        company_info_block.push_str("CompanyInfo:\n");
                        company_info_block.push_str(&format!("- {}\n", text));
                        if let Ok(vs) = model.embed(&[text.clone()]) {
                            if let Some(vec0) = vs.get(0) {
                                let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                                let item = crate::qa::vector::lance_store::Chunk {
                                    id: format!("doc:{}:company_info:{}", pipeline, epoch),
                                    kind: "doc".to_string(),
                                    namespace: "company".to_string(),
                                    field: None,
                                    text: text.clone(),
                                    vector: vec0.clone(),
                                    meta: serde_json::json!({"source":"user"}),
                                    epoch,
                                };
                                let _ = store.upsert(&[item]).await;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    registry.register(SqlSampleTool { ctx: ctx.clone() });
    registry.register(VectQueryTool);
    registry.register(crate::qa::tools::ask_user::AskUserTool);
    let actx = AgentCtx {
        top_k: 100,
        per_step_timeout_secs: 10,
        max_steps: 10,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        agent_name: Some("ask".to_string()),
        dataset_candidates: Vec::new(),
    };
    let base_sys = system_prompt();
    let mut sys = base_sys;
    // Add TimeContext so the agent always knows now (UTC) and user's local timezone
    let now_utc = chrono::Utc::now().to_rfc3339();
    let now_local = chrono::Local::now();
    let local_iso = now_local.to_rfc3339();
    let local_offset = now_local.offset().to_string();
    sys = format!(
        "{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})",
        sys, now_utc, local_iso, local_offset
    );
    if !embeds_block.is_empty() { sys = format!("{}\n\n{}", sys, embeds_block); }
    if !company_info_block.is_empty() { sys = format!("{}\n\n{}", sys, company_info_block); }
    let tools = tool_card();
    // Provide a stable thread id (UUID v4) so we can inspect steps later instead of re-running SQL.
    let thread_id = Uuid::new_v4().to_string();
    let actx = AgentCtx { thread_id: Some(thread_id.clone()), ..actx };
    // Interactive loop: run agent until it needs user input or reaches final
    let mut thread_id = thread_id;
    let mut result: Option<crate::qa::session::ThreadResult> = None;
    loop {
        let mut actx2 = actx.clone();
        actx2.thread_id = Some(thread_id.clone());
        match Agent::run_until_block(&registry, &actx2, &sys, &tools, question).await.map_err(|e| e.to_string())? {
            crate::qa::agent::RunOutcome::Final { thread_id: tid, result: r } => { thread_id = tid; result = Some(r); break; }
            crate::qa::agent::RunOutcome::AwaitUser { thread_id: tid, prompt } => {
                thread_id = tid;
                println!("{}", prompt);
                let mut buf = String::new();
                let _ = std::io::stdin().read_line(&mut buf);
                let text = buf.trim().to_string();
                if !text.is_empty() {
                    // Embed and upsert to LanceDB
                    if let Some(qv) = qvec_opt.as_ref() {
                        let cfg = crate::llm::config_from_env();
                        let model = crate::llm::create_llm(&cfg);
                        if let Ok(vs) = model.embed(&[text.clone()]) {
                            if let Some(vec0) = vs.get(0) {
                                let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                                let store = crate::qa::vector::lance_store::LanceDbStore::new(pipeline);
                                let item = crate::qa::vector::lance_store::Chunk {
                                    id: format!("doc:{}:company_info:{}", pipeline, epoch),
                                    kind: "doc".to_string(),
                                    namespace: "company".to_string(),
                                    field: None,
                                    text: text.clone(),
                                    vector: vec0.clone(),
                                    meta: serde_json::json!({"source":"user"}),
                                    epoch,
                                };
                                let _ = store.upsert(&[item]).await;
                            }
                        }
                    }
                    // Append user answer into thread
                    let store = crate::qa::session::ThreadStore::new();
                    let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
                        action: "user".to_string(),
                        args: serde_json::json!({"text": text}),
                        observation: serde_json::json!({"ok": true}),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: Some("ask".to_string()),
                    }).await;
                }
                // Continue loop to let agent resume
            }
            crate::qa::agent::RunOutcome::AwaitApproval { thread_id: tid, prompt } => {
                thread_id = tid;
                println!("{}", prompt);
                let mut buf = String::new();
                let _ = std::io::stdin().read_line(&mut buf);
                let text = buf.trim().to_string();
                if !text.is_empty() {
                    let store = crate::qa::session::ThreadStore::new();
                    let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
                        action: "user".to_string(),
                        args: serde_json::json!({"text": text}),
                        observation: serde_json::json!({"ok": true}),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: Some("ask".to_string()),
                    }).await;
                }
                // Continue loop to let agent resume
            }
        }
    }
    let result = result.ok_or_else(|| "No result".to_string())?;
    // Always print the final thread JSON for visibility in CLI mode
    {
        let store = crate::qa::session::ThreadStore::new();
        if let Some(log) = store.get(&thread_id).await {
            if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                println!("{}", pretty);
            }
        }
    }

    // Try to reuse the last successful run_sql observation from the agent thread to avoid re-running.
    let mut header: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    {
        let store = crate::qa::session::ThreadStore::new();
        if let Some(log) = store.get(&thread_id).await {
            for step in log.steps.iter().rev() {
                if step.action == "run_sql" {
                    let obs = &step.observation;
                    if obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        header = obs.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()).unwrap_or_default();
                        rows = obs.get("rows").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default();
                        break;
                    }
                }
            }
        }
    }
    // If still empty and we have SQL, run once as a fallback.
    if header.is_empty() && rows.is_empty() {
        if let Some(sql) = result.sql.clone() {
            if let Ok(obs) = registry.call("run_sql", json!({"sql": sql}), &actx).await {
                if obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    header = obs.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()).unwrap_or_default();
                    rows = obs.get("rows").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default();
                }
            }
        }
    }

    // Company information: query doc embeddings; if missing, ask user and upsert.
    let company_info: String = company_info_block.clone();

    // Catalog context from the first referenced dataset in SQL (if any).
    let catalog_context: String = {
        fn first_fqn(sql: &str) -> Option<(String, String)> {
            // naive scan for token containing a single '.' with alnum/underscore on both sides
            let delimiters: &[char] = &[' ', '\n', '\t', ',', ';', '(', ')'];
            for tok in sql.split(delimiters) {
                if let Some(dot) = tok.find('.') {
                    let (l, r) = (&tok[..dot], &tok[dot+1..]);
                    if !l.is_empty() && !r.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                        return Some((l.to_string(), r.to_string()));
                    }
                }
            }
            None
        }
        let mut ctx_line = String::new();
        if let Some(sql) = result.sql.as_ref() {
            if let Some((p, ns)) = first_fqn(sql) {
                if let Some(entry) = crate::sql::registry::find_entry(&p, &ns).await {
                    if !entry.catalog_key.is_empty() {
                        if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                            let desc = val.get("description").and_then(|x| x.as_str()).unwrap_or_default();
                            if !desc.is_empty() {
                                ctx_line = format!("Dataset {}.{}: {}", p, ns, desc);
                            }
                        }
                    }
                }
            }
        }
        ctx_line
    };

    // Build a concise business-style summary using LLM ONLY if we have data.
    let final_summary: Option<String> = if !header.is_empty() && !rows.is_empty() {
        let cfg = crate::llm::config_from_env();
        let llm = crate::llm::create_llm(&cfg);
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Question: {}", question));
        if !result.answer.trim().is_empty() {
            lines.push(format!("AgentAnswer: {}", result.answer.trim()));
        }
        if let Some(sql) = result.sql.as_ref() {
            lines.push(format!("SQL: {}", sql.replace('\n', " ")));
        }
        let max_rows = rows.len().min(20);
        let display_rows = &rows[..max_rows];
        let table_json = serde_json::json!({
            "header": header,
            "rows": display_rows,
        });
        lines.push(format!("Data: {}", table_json.to_string()));
        if !catalog_context.is_empty() {
            lines.push(format!("Catalog: {}", catalog_context));
        }
        if !company_info.trim().is_empty() {
            lines.push(format!("CompanyInfo: {}", company_info.trim()));
        }
        let prompt = format!(
            "You are a data analyst. Write a short, business-friendly answer for executives.\n\
            Rules:\n- Be concise (≤ 2 sentences), plain text only.\n- If the data is a time series (date + metric), comment on trend/growth, and compare to the prior period when possible using the provided rows only.\n- Do not fabricate numbers; only use provided data.\n- If insufficient data for trends, state the key facts only.\n- Incorporate any relevant Catalog or CompanyInfo context to shape the message, but do not invent specifics.\n\nContext:\n{}\n\nAnswer:",
            lines.join("\n")
        );
        match tokio::task::spawn_blocking({ let llm2 = llm.clone(); let p = prompt.clone(); move || llm2.chat(&[crate::llm::ChatMessage { role: "user".into(), content: p }]) }).await {
            Ok(Ok(text)) => {
                let t = text.trim();
                if !t.is_empty() { Some(t.to_string()) } else { None }
            }
            _ => None
        }
    } else {
        None
    };

    if let Some(s) = final_summary {
        return Ok(s);
    }

    // Otherwise, if the agent produced SQL, execute it and synthesize a concise plain-text answer.
    if let Some(sql) = result.sql.clone() {
        if let Ok(obs) = registry.call("run_sql", json!({"sql": sql}), &actx).await {
            if obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                let header: Vec<String> = obs.get("header")
                    .and_then(|h| serde_json::from_value(h.clone()).ok())
                    .unwrap_or_default();
                let rows: Vec<Vec<String>> = obs.get("rows")
                    .and_then(|r| serde_json::from_value(r.clone()).ok())
                    .unwrap_or_default();
                if !rows.is_empty() {
                    let first = &rows[0];
                    // Heuristics to produce a readable single sentence
                    let plain = if first.len() == 1 {
                        // Single value result
                        first[0].clone()
                    } else if first.len() == 2 {
                        // Often time bucket + metric; prefer "<metric> on <time>"
                        let h0 = header.get(0).cloned().unwrap_or_else(|| "col1".to_string());
                        let h1 = header.get(1).cloned().unwrap_or_else(|| "col2".to_string());
                        let v0 = first.get(0).cloned().unwrap_or_default();
                        let v1 = first.get(1).cloned().unwrap_or_default();
                        // If the second looks numeric, treat as metric
                        if v1.parse::<f64>().is_ok() {
                            format!("{} {} on {}", v1, h1.replace('_', " "), v0)
                        } else {
                            format!("{} {} and {} {}", v0, h0.replace('_', " "), v1, h1.replace('_', " "))
                        }
                    } else {
                        // Generic: join header=value for first row
                        let pairs: Vec<String> = header.iter().zip(first.iter())
                            .map(|(h, v)| format!("{}={}", h, v))
                            .collect();
                        format!("Result: {}", pairs.join(", "))
                    };
                    return Ok(plain);
                }
            }
        }
    }

    // Fallback: if no rows, return a conservative message, otherwise return agent's plain answer.
    if header.is_empty() || rows.is_empty() {
        return Ok("No result rows were returned for the requested period or query. Please refine the question or adjust the time window.".to_string());
    }
    Ok(result.answer)
}


