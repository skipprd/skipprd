use datafusion::prelude::SessionContext;
use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool};
use std::cmp::Ordering;
use serde_json::json;

pub async fn run(question: &str, pipeline: &str, namespace: Option<&str>) -> Result<String, String> {
    let ctx = SessionContext::new();
    // Auto-register all pipelines/namespaces so queries don't require a specific pipeline flag
    {
        let pipelines = crate::helpers::configuration::Config::get_pipelines();
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
    {
        let cfg = crate::llm::config_from_env();
        let model = crate::llm::create_llm(&cfg);
        let vecs = model.embed(&[question.to_string()]).map_err(|e| e.to_string()).unwrap_or_default();
        if let Some(qvec) = vecs.get(0) {
            let mut all_hits: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
            let pipelines = crate::helpers::configuration::Config::get_pipelines();
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
    }
    let mut registry = ToolRegistry::new();
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    registry.register(SqlSampleTool { ctx: ctx.clone() });
    registry.register(VectQueryTool);
    let actx = AgentCtx {
        pipeline: pipeline.to_string(),
        namespace: namespace.map(|s| s.to_string()),
        top_k: 30,
        per_step_timeout_secs: 10,
        max_steps: 6,
        thread_id: None,
    };
    let base_sys = system_prompt();
    let sys = if embeds_block.is_empty() { base_sys } else { format!("{}\n\n{}", base_sys, embeds_block) };
    let tools = tool_card();
    let result = Agent::run(&registry, &actx, &sys, &tools, question).await?;

    // If the agent produced SQL, execute it and synthesize a concise plain-text answer.
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

    // Fallback to whatever the agent provided (should still be plain text).
    Ok(result.answer)
}


