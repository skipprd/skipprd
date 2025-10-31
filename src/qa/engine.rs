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

    // LLM-chained planning: dataset -> fields -> SQL -> validation/repair -> English answer
    // 2a) If more than one candidate, ask LLM to choose the dataset; else use the single
    let chosen_ns = if candidates.len() == 1 {
        candidates[0].namespace.clone()
    } else {
        let mut cand_lines: Vec<String> = Vec::new();
        for c in &candidates {
            let mut line = format!("namespace:{}", c.namespace);
            if let Some(desc) = &c.root_description { if !desc.is_empty() { line.push_str(&format!(" description:{}", desc)); } }
            if !c.fields.is_empty() { line.push_str(&format!(" fields:{}", c.fields.iter().take(20).cloned().collect::<Vec<_>>().join(","))); }
            cand_lines.push(line);
        }
        let prompt = prompts::dataset_selection(&cand_lines.join("\n"), question);
        println!("{} ASK: LLM dataset selection...", chrono::Utc::now().to_rfc3339());
        let resp = model.chat(&[ChatMessage { role: "user".into(), content: prompt }]).unwrap_or_default();
        let picked = serde_json::from_str::<serde_json::Value>(&resp).ok()
            .and_then(|v| v.get("namespace").and_then(|x| x.as_str()).map(|s| s.to_string()));
        let ns = picked.unwrap_or_else(|| candidates[0].namespace.clone());
        println!("{} ASK: namespace '{}' selected", chrono::Utc::now().to_rfc3339(), ns);
        ns
    };

    // 2b) Register the chosen namespace
    PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&chosen_ns);
    Config::init().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), register_namespace_view(&ctx, &chosen_ns)).await;
    let df = match ctx.table(&chosen_ns).await { Ok(df) => df, Err(e) => { return Ok(Answer { text: format!("Dataset '{}' is not available: {}", chosen_ns, e), followup: None }); } };

    // Collect catalog fields for the namespace
    let mut fields_ctx: Vec<String> = Vec::new();
    let mut field_names: Vec<String> = Vec::new();
    let cat_sql = format!("SELECT field, coalesce(role,''), coalesce(synonyms,''), coalesce(description,'') FROM catalog WHERE namespace='{}' ORDER BY field", chosen_ns.replace("'", "''"));
    if let Ok(dfc) = ctx.sql(&cat_sql).await { if let Ok(batches) = dfc.collect().await { for b in batches { for r in 0..b.num_rows() {
        let f = crate::sql::tui::value_to_string(b.column(0).as_ref(), r);
        let role = crate::sql::tui::value_to_string(b.column(1).as_ref(), r);
        let syn = crate::sql::tui::value_to_string(b.column(2).as_ref(), r);
        let desc = crate::sql::tui::value_to_string(b.column(3).as_ref(), r);
        fields_ctx.push(format!("name={} role={} syn={} desc={}", f, role, syn, desc));
        field_names.push(f);
    } } } }

    // Collect schema types for the namespace
    let mut schema_ctx: Vec<String> = Vec::new();
    for f in df.schema().fields() { schema_ctx.push(format!("{}:{:?}", f.name(), f.data_type())); }

    // 3) Field selection (group and optional time), via LLM
    let field_names_json = serde_json::to_string(&field_names).unwrap_or("[]".to_string());
    let fs_prompt = prompts::field_selection_with_names(&fields_ctx.join("\n"), &schema_ctx.join(", "), &field_names_json, question, opts.top_k);
    println!("{} ASK: LLM field selection...", chrono::Utc::now().to_rfc3339());
    let fs_resp = model.chat(&[ChatMessage { role: "user".into(), content: fs_prompt }]).unwrap_or_default();
    let (group_field, time_field_opt) = {
        let mut parsed = serde_json::from_str::<serde_json::Value>(&fs_resp).ok();
        if parsed.is_none() {
            // one retry: ask for strict JSON only
            let retry_prompt = format!("Return only STRICT JSON with keys groupField,timeField,filters. Choose groupField from this array exactly: {}. Previous was:\n{}", field_names_json, fs_resp);
            let retry = model.chat(&[ChatMessage { role: "user".into(), content: retry_prompt }]).unwrap_or_default();
            parsed = serde_json::from_str::<serde_json::Value>(&retry).ok();
        }
        let pf = parsed.unwrap_or(serde_json::json!({}));
        let gf = pf.get("groupField").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let tf = pf.get("timeField").and_then(|x| if x.is_string() { x.as_str() } else { None }).map(|s| s.to_string());
        (gf, tf)
    };
    if group_field.is_empty() {
        // final attempt: force a choice from candidate names
        let must_pick_prompt = format!("You MUST pick a groupField from this array to best answer the question. Respond STRICT JSON only with groupField,timeField,filters. candidates={} question=\"{}\"", field_names_json, question);
        let forced = model.chat(&[ChatMessage { role: "user".into(), content: must_pick_prompt }]).unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&forced) {
            if let Some(gf) = v.get("groupField").and_then(|x| x.as_str()) { if !gf.is_empty() {
                let tf = v.get("timeField").and_then(|x| if x.is_string() { x.as_str() } else { None }).map(|s| s.to_string());
                // overwrite with forced selection
                let group_field_forced = gf.to_string();
                return {
                    // resume flow using the forced selection by regenerating SQL JSON prompt below
                    // fall through by reusing variables via a scoped block
                    let sqlg_prompt = prompts::sql_generation_json(&chosen_ns, question, &group_field_forced, tf.as_deref(), opts.top_k);
                    println!("{} ASK: LLM SQL generation...", chrono::Utc::now().to_rfc3339());
                    let mut sql_json = model.chat(&[ChatMessage { role: "user".into(), content: sqlg_prompt }]).unwrap_or_default();
                    let mut sql_str = serde_json::from_str::<serde_json::Value>(&sql_json).ok()
                        .and_then(|v| v.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string()))
                        .unwrap_or_default();

                    // Execute with validation/repair loop (up to 2 retries)
                    let mut data_rows: Vec<String> = Vec::new();
                    let mut error_text: Option<String> = None;
                    for attempt in 0..3 {
                        println!("{} ASK: executing SQL (attempt {})...", chrono::Utc::now().to_rfc3339(), attempt + 1);
                        match ctx.sql(&sql_str).await {
                            Ok(dfout) => match dfout.collect().await {
                                Ok(batches) => {
                                    if let Some(first) = batches.first() {
                                        let headers: Vec<String> = first.schema().fields().iter().map(|f| f.name().to_string()).collect();
                                        data_rows.push(headers.join(","));
                                    }
                                    for b in batches { for row in 0..b.num_rows() { let mut row_vals: Vec<String> = Vec::new(); for col in 0..b.num_columns() { row_vals.push(crate::sql::tui::value_to_string(b.column(col).as_ref(), row)); } data_rows.push(row_vals.join(",")); } }
                                    error_text = None; break;
                                }
                                Err(e) => { error_text = Some(format!("collect_error: {}", e)); }
                            },
                            Err(e) => { error_text = Some(format!("compile_error: {}", e)); }
                        }
                        if let Some(err) = &error_text {
                            let repair_prompt = prompts::sql_repair(&sql_json, err, &schema_ctx.join(", "));
                            sql_json = model.chat(&[ChatMessage { role: "user".into(), content: repair_prompt }]).unwrap_or_default();
                            sql_str = serde_json::from_str::<serde_json::Value>(&sql_json).ok()
                                .and_then(|v| v.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string()))
                                .unwrap_or_default();
                        }
                    }
                    if let Some(err) = error_text { return Ok(Answer { text: format!("SQL failed after retries: {}", err), followup: None }); }

                    let mut extra_ctx = String::new();
                    if let Some(tf_name) = tf.as_ref() {
                        if df.schema().fields().iter().any(|f| f.name() == tf_name || tf_name.contains('.')) {
                            let min_sql = format!("SELECT MIN({}) AS earliest FROM {}", tf_name, chosen_ns);
                            if let Ok(dft) = ctx.sql(&min_sql).await { if let Ok(bs) = dft.collect().await { if let Some(b) = bs.first() { let v = crate::sql::tui::value_to_string(b.column(0).as_ref(), 0); if !v.is_empty() { extra_ctx = format!("earliest_time={}", v); } } } }
                        }
                    }

                    let mut meta_ctx_lines: Vec<String> = Vec::new();
                    if let Some(c) = candidates.iter().find(|c| c.namespace == chosen_ns) { if let Some(desc) = &c.root_description { if !desc.is_empty() { meta_ctx_lines.push(format!("description:{}", desc)); } } }
                    meta_ctx_lines.push(format!("namespace:{}", chosen_ns));
                    let rows_ctx = data_rows.join("\n");
                    let ans_prompt = prompts::english_synthesis(&meta_ctx_lines.join("\n"), &sql_json, &rows_ctx, &extra_ctx, question);
                    println!("{} ASK: LLM answer synthesis...", chrono::Utc::now().to_rfc3339());
                    let answer_text = model.chat(&[ChatMessage { role: "user".into(), content: ans_prompt }]).unwrap_or_default();
                    println!("{} ASK: done", chrono::Utc::now().to_rfc3339());
                    Ok(Answer { text: answer_text.trim().to_string(), followup: None })
                };
            } }
        }
        return Ok(Answer { text: "I could not determine a grouping field from the catalog. Try rephrasing the question.".to_string(), followup: None });
    }

    // 4) SQL generation (JSON) via LLM
    let sqlg_prompt = prompts::sql_generation_json(&chosen_ns, question, &group_field, time_field_opt.as_deref(), opts.top_k);
    println!("{} ASK: LLM SQL generation...", chrono::Utc::now().to_rfc3339());
    let mut sql_json = model.chat(&[ChatMessage { role: "user".into(), content: sqlg_prompt }]).unwrap_or_default();
    // sanitize potential code fences and non-JSON wrappers
    let mut parse_attempts = 0;
    let mut sql_str = String::new();
    loop {
        parse_attempts += 1;
        let mut s = sql_json.trim().to_string();
        if s.starts_with("```") {
            if let Some(pos) = s.find('\n') { s = s[pos+1..].to_string(); }
            if s.ends_with("```") { s = s.trim_end_matches('`').trim_end_matches('`').trim_end_matches('`').to_string(); }
        }
        if s.to_uppercase().starts_with("SELECT ") {
            // Model returned bare SQL; wrap it
            sql_str = s;
            break;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(ss) = v.get("sql").and_then(|x| x.as_str()) { sql_str = ss.to_string(); }
        }
        if !sql_str.trim().is_empty() && sql_str.trim_start().to_uppercase().starts_with("SELECT ") { break; }
        if parse_attempts >= 2 {
            // ask for strict JSON re-emission
            let tighten = format!("Return STRICT JSON only: {{\"sql\": \"<single SELECT that starts with SELECT>\"}}. No prose. Previous=\n{}", sql_json);
            sql_json = model.chat(&[ChatMessage { role: "user".into(), content: tighten }]).unwrap_or_default();
            // one last parse on next loop
        } else {
            // retry once immediately with clarifier
            let clarify = "Respond with STRICT JSON only: {\"sql\": \"...\"}. SQL must start with SELECT.".to_string();
            sql_json = model.chat(&[ChatMessage { role: "user".into(), content: clarify }]).unwrap_or_default();
        }
        if parse_attempts > 3 { break; }
    }
    if sql_str.trim().is_empty() {
        return Ok(Answer { text: "LLM did not return a usable SQL statement. Please rephrase the question and try again.".to_string(), followup: None });
    }

    // 5) Execute with validation/repair loop (up to 2 retries)
    let mut data_rows: Vec<String> = Vec::new();
    let mut error_text: Option<String> = None;
    for attempt in 0..3 {
        if !sql_str.to_lowercase().contains(" from ") || !sql_str.contains(&chosen_ns) {
            // let LLM fix it rather than hardcoding
        }
        println!("{} ASK: executing SQL (attempt {})...", chrono::Utc::now().to_rfc3339(), attempt + 1);
        match ctx.sql(&sql_str).await {
            Ok(dfout) => match dfout.collect().await {
                Ok(batches) => {
                    // Build CSV-like with header
                    if let Some(first) = batches.first() {
                        let headers: Vec<String> = first.schema().fields().iter().map(|f| f.name().to_string()).collect();
                        data_rows.push(headers.join(","));
                    }
                    for b in batches {
                        for row in 0..b.num_rows() {
                            let mut row_vals: Vec<String> = Vec::new();
                            for col in 0..b.num_columns() { row_vals.push(crate::sql::tui::value_to_string(b.column(col).as_ref(), row)); }
                            data_rows.push(row_vals.join(","));
                        }
                    }
                    error_text = None; break;
                }
                Err(e) => { error_text = Some(format!("collect_error: {}", e)); }
            },
            Err(e) => { error_text = Some(format!("compile_error: {}", e)); }
        }
        if let Some(err) = &error_text {
            let repair_prompt = prompts::sql_repair(&sql_json, err, &schema_ctx.join(", "));
            sql_json = model.chat(&[ChatMessage { role: "user".into(), content: repair_prompt }]).unwrap_or_default();
            // re-parse with the same sanitizer
            let mut s = sql_json.trim().to_string();
            if s.starts_with("```") { if let Some(pos) = s.find('\n') { s = s[pos+1..].to_string(); } if s.ends_with("```") { s = s.trim_end_matches('`').trim_end_matches('`').trim_end_matches('`').to_string(); } }
            if s.to_uppercase().starts_with("SELECT ") {
                sql_str = s;
            } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) { if let Some(ss) = v.get("sql").and_then(|x| x.as_str()) { sql_str = ss.to_string(); } }
            if sql_str.trim().is_empty() { continue; }
        }
    }
    if let Some(err) = error_text { return Ok(Answer { text: format!("SQL failed after retries: {}", err), followup: None }); }

    // Optional extra context (e.g., earliest time), only if time_field was selected and exists
    let mut extra_ctx = String::new();
    if let Some(tf) = time_field_opt.as_ref() {
        let exists = df.schema().fields().iter().any(|f| f.name() == tf);
        if exists {
            let min_sql = format!("SELECT MIN({}) AS earliest FROM {}", tf, chosen_ns);
            if let Ok(dft) = ctx.sql(&min_sql).await { if let Ok(bs) = dft.collect().await { if let Some(b) = bs.first() { let v = crate::sql::tui::value_to_string(b.column(0).as_ref(), 0); if !v.is_empty() { extra_ctx = format!("earliest_time={}", v); } } } }
        }
    }

    // 6) English synthesis (optional context)
    let mut meta_ctx_lines: Vec<String> = Vec::new();
    if let Some(c) = candidates.iter().find(|c| c.namespace == chosen_ns) { if let Some(desc) = &c.root_description { if !desc.is_empty() { meta_ctx_lines.push(format!("description:{}", desc)); } } }
    meta_ctx_lines.push(format!("namespace:{}", chosen_ns));
    let rows_ctx = data_rows.join("\n");
    let ans_prompt = prompts::english_synthesis(&meta_ctx_lines.join("\n"), &sql_json, &rows_ctx, &extra_ctx, question);
    println!("{} ASK: LLM answer synthesis...", chrono::Utc::now().to_rfc3339());
    let answer_text = model.chat(&[ChatMessage { role: "user".into(), content: ans_prompt }]).unwrap_or_default();
    println!("{} ASK: done", chrono::Utc::now().to_rfc3339());
    return Ok(Answer { text: answer_text.trim().to_string(), followup: None });

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


