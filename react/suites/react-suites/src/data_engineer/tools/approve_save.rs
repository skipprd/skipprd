use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;
use tracing::info;

use react_core::agent::AgentCtx;
use react_core::session::ToolStepStatus;
use react_core::tools::Tool;
use crate::data_engineer::references::DatasetRef;
use crate::data_engineer::progress_controller::ExecutionState;
use crate::data_engineer::state_manager;

// NOTE: Engine-agnostic ReAct: no DataFusion/SessionContext usage here.
pub struct ApproveAndSaveArtifactTool;

fn encode_key_component(s: &str) -> String {
    // Match Keyspace / provider encoding: keep a conservative safe set; percent-encode the rest.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn resolve_single_dataset_id_from_args(args: &Value) -> Result<Option<DatasetRef>, String> {
    // Hard cutover: accept explicit dataset_id only.
    if let Some(s) = args.get("dataset_id").and_then(|x| x.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            return DatasetRef::parse(t)
                .ok_or_else(|| {
                    format!(
                        "invalid dataset_id '{t}'. Expected <catalog>.<schema>.<table> (e.g. AwsDataCatalog.test_raw.raw_customers)."
                    )
                })
                .map(Some);
        }
    }
    Ok(None)
}

#[async_trait]
impl Tool for ApproveAndSaveArtifactTool {
    fn name(&self) -> &'static str {
        "approve_and_save_artifact"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let kind = args.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        if kind != "model" && kind != "metric" {
            return Err("kind must be 'model' or 'metric'".to_string());
        }
        let name = args
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            return Err("name required".to_string());
        }
        let content = args
            .get("content")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if content.trim().is_empty() {
            return Err("content required".to_string());
        }
        if args.get("preview_diff").is_some() {
            return Err(
                "preview_diff is no longer supported; remove it from the request".to_string(),
            );
        }
        let mut warnings: Vec<String> = Vec::new();

        let explicit_dataset_id = resolve_single_dataset_id_from_args(&args)?;

        // Hard cutover: require explicit dataset_id; do not infer from thread history.
        let mut candidates: Vec<DatasetRef> = Vec::new();
        if let Some(ref ds) = explicit_dataset_id {
            candidates.push(ds.clone());
        }

        let mut dataset_id: String = if let Some(ds) = explicit_dataset_id {
            ds.fqn()
        } else if candidates.is_empty() {
            return Err(
                "No resolved datasets found. First, resolve dataset candidates via vect_query(scope:\"dataset\"), record them, then retry save."
                    .to_string(),
            );
        } else if kind == "model" {
            let lc = content.to_lowercase();
            let mut referenced: Vec<DatasetRef> = Vec::new();
            for ds in candidates.iter() {
                let ds_lc = ds.fqn().to_lowercase();
                let mut ok = lc.contains(&ds_lc);
                if !ok {
                    let db_table = format!("{}.{}", ds.schema, ds.table).to_lowercase();
                    let src1 = format!("source('{}','{}')", ds.schema, ds.table);
                    let src2 = format!("source(\"{}\",\"{}\")", ds.schema, ds.table);
                    let src3 = format!("source('{}', '{}')", ds.schema, ds.table);
                    let src4 = format!("source(\"{}\", \"{}\")", ds.schema, ds.table);
                    ok = lc.contains(&db_table)
                        || lc.contains(&src1)
                        || lc.contains(&src2)
                        || lc.contains(&src3)
                        || lc.contains(&src4);
                }
                if ok {
                    referenced.push(ds.clone());
                }
            }
            if referenced.is_empty() {
                let list = candidates
                    .iter()
                    .map(|d| d.fqn())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "Model SQL must reference at least one resolved dataset (FQN or dbt source). Use one of: {}",
                    list
                ));
            }
            referenced[0].fqn()
        } else {
            candidates[0].fqn()
        };

        // For metrics, persist a top comment documenting the assumed dataset
        let mut content_final = if kind == "metric" {
            ensure_dataset_comment(&content, &dataset_id)
        } else {
            content.clone()
        };

        // Update-in-place: if a focused artifact exists in typed execution state, enforce writing to that exact key.
        let mut name_final = name.clone();
        if let Some(tid) = ctx.thread_id.as_ref() {
            if let Some(store) = ctx.thread_store.as_ref() {
                let es = state_manager::load_execution_state(store, tid)
                    .await
                    .unwrap_or_else(ExecutionState::new);
                if let Some(focus) = es.artifact_focus.as_ref() {
                    if focus.exists {
                        let fk = focus.kind.as_deref().unwrap_or("");
                        if fk != kind {
                            return Err(format!(
                                "Focused artifact kind is '{}'; cannot save kind '{}'. Update the focused artifact in place.",
                                fk, kind
                            ));
                        }
                        let fname = focus.name.as_deref().unwrap_or("").trim();
                        let fds = focus.dataset_id.as_deref().unwrap_or("").trim();
                        if !fname.is_empty() && !fds.is_empty() {
                            name_final = fname.to_string();
                            dataset_id = fds.to_string();
                            if kind == "metric" {
                                content_final = ensure_dataset_comment(&content_final, &dataset_id);
                            }
                        }
                    }
                }
            }
        }

        let (current_key, content_type) = match kind {
            "model" => {
                let base = ctx
                    .keyspace
                    .dbt_prefix(&ctx.scope)
                    .trim_end_matches('/')
                    .to_string();
                let dir = encode_key_component(&dataset_id);
                let current = format!("{}/models/{}/{}.sql", base, dir, name_final);
                (current, "text/sql")
            }
            _ => {
                let base = ctx
                    .keyspace
                    .dbt_prefix(&ctx.scope)
                    .trim_end_matches('/')
                    .to_string();
                let dir = encode_key_component(&dataset_id);
                let current = format!("{}/metrics/{}/{}.yaml", base, dir, name_final);
                (current, "text/yaml")
            }
        };

        // Ensure minimal dbt project scaffolding exists before first save
        let dbt = ctx
            .dbt
            .as_ref()
            .ok_or_else(|| "dbt provider missing".to_string())?;
        if let Err(e) = dbt.ensure_minimal_project(&ctx.scope).await {
            return Ok(
                serde_json::json!({"ok": false, "error": format!("failed to ensure minimal dbt project: {e}")}),
            );
        }

        let rel_path = current_key
            .strip_prefix(
                &(ctx
                    .keyspace
                    .dbt_prefix(&ctx.scope)
                    .trim_end_matches('/')
                    .to_string()
                    + "/"),
            )
            .unwrap_or(&current_key)
            .to_string();
        let outcome = crate::data_engineer::mutation_gateway::replace_file_content(
            ctx,
            None,
            &rel_path,
            &content_final,
            crate::data_engineer::mutation_gateway::ExpectedBase::Any,
            content_type,
        )
        .await?;
        let status = if outcome.existed {
            react_core::session::ArtifactSaveStatus::Modified
        } else {
            react_core::session::ArtifactSaveStatus::Added
        };

        info!("Artifact saved: kind={} key={}", kind, current_key);

        // Upsert/update embedding for this artifact (immediate availability)
        {
            let llm = ctx.llm.clone();
            let vector = ctx
                .vector
                .as_ref()
                .ok_or_else(|| "vector provider missing".to_string())?;
            if let Ok(vecs) = llm.embed(&[content_final.clone()]) {
                if let Some(v) = vecs.get(0) {
                    let atype = if kind == "model" {
                        "dbt_model"
                    } else {
                        "dbt_metricflow"
                    };
                    let ds_id = dataset_id.clone();
                    let id = format!(
                        "artifact:{}:{}:{}",
                        if kind == "model" { "model" } else { "metric" },
                        ds_id,
                        name_final
                    );
                    let chunk = react_core::providers::VectorChunk {
                        id,
                        kind: "artifact".to_string(),
                        dataset_id: dataset_id.clone(),
                        field: None,
                        text: content_final.clone(),
                        vector: v.clone(),
                        meta: serde_json::json!({"type": atype, "path": current_key}),
                        epoch: chrono::Utc::now().timestamp() as u64,
                    };
                    if let Err(e) = vector.upsert(&ctx.scope, &[chunk]).await {
                        warnings.push(format!("vector upsert failed: {}", e));
                    }
                }
            }
        }

        // Log step into thread if available
        if let Some(tid) = ctx.thread_id.as_ref() {
            let store = ctx
                .thread_store
                .as_ref()
                .ok_or_else(|| "thread_store not configured".to_string())?;
            let agent = ctx
                .agent_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let _ = store
                .append_step(
                    tid,
                    react_core::session::ThreadStep::ArtifactSaved {
                        kind: react_core::session::ArtifactKind::from(kind.to_string()),
                        name: name.to_string(),
                        dataset_id: Some(dataset_id.clone()),
                        key: current_key.clone(),
                        status: status.clone(),
                        lines_added: outcome.lines_added as u64,
                        lines_removed: outcome.lines_removed as u64,
                        observation: react_core::session::Observation::ok(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;

            // Persist typed artifact focus for deterministic subsequent updates.
            let _ = state_manager::mutate_execution_state(store, tid, |es| {
                es.set_artifact_focus(
                    Some(kind.to_string()),
                    Some(name_final.clone()),
                    Some(dataset_id.clone()),
                    true,
                );
            })
            .await;

            // Immediately validate the DBT project and refresh compiled views to guarantee consistency
            let s3_prefix = ctx.keyspace.dbt_prefix(&ctx.scope);
            let validate_tool = crate::data_engineer::tools::dbt_validate::DbtValidateTool {
                datasets: None,
                catalog: None,
            };
            let project_name = format!("{}_project", ctx.scope.project_id.replace('/', "_"));
            let args = json!({
                "project_name": project_name,
                "s3_prefix": s3_prefix,
                "build": false
            });
            match validate_tool.call(args, ctx).await {
                Ok(obs) => {
                    info!(
                        "approve_and_save_artifact: dbt_validate observation: {:?}",
                        obs
                    );
                    let obs_norm = react_core::session::ToolObservation::normalize(obs);
                    let tool_id = uuid::Uuid::new_v4().to_string();
                    let _ = store
                        .append_step(
                            tid,
                            react_core::session::ThreadStep::ToolEnd {
                                tool_id,
                                name: "dbt_validate".to_string(),
                                clean_name: "Validate DBT".to_string(),
                                args: serde_json::json!({"s3_prefix": s3_prefix, "build": true}),
                                status: if obs_norm.ok {
                                    ToolStepStatus::Ok
                                } else {
                                    ToolStepStatus::Failed
                                },
                                payload: None,
                                ctx: None,
                                observation: obs_norm,
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: agent.clone(),
                            },
                        )
                        .await;
                }
                Err(e) => {
                    info!("approve_and_save_artifact: dbt_validate failed: {}", e);
                    let obs_norm = react_core::session::ToolObservation::normalize(
                        serde_json::json!({"ok": false, "errors": [e]}),
                    );
                    let tool_id = uuid::Uuid::new_v4().to_string();
                    let _ = store
                        .append_step(
                            tid,
                            react_core::session::ThreadStep::ToolEnd {
                                tool_id,
                                name: "dbt_validate".to_string(),
                                clean_name: "Validate DBT".to_string(),
                                args: serde_json::json!({"s3_prefix": s3_prefix, "build": true}),
                                status: ToolStepStatus::Failed,
                                payload: None,
                                ctx: None,
                                observation: obs_norm,
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: agent.clone(),
                            },
                        )
                        .await;
                }
            }
        }

        Ok(
            serde_json::json!({"ok": true, "key": current_key, "status": status.as_str(), "lines_added": outcome.lines_added, "lines_removed": outcome.lines_removed, "warnings": warnings}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_single_dataset_id_from_args_accepts_dataset_id() {
        let args = serde_json::json!({"dataset_id":"AwsDataCatalog.test_raw.raw_customers"});
        let got = resolve_single_dataset_id_from_args(&args).expect("ok");
        assert_eq!(
            got.map(|d| d.fqn()),
            Some("AwsDataCatalog.test_raw.raw_customers".to_string())
        );
    }

    #[test]
    fn resolve_single_dataset_id_from_args_ignores_legacy_dataset_ids_shape() {
        let args = serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_customers"]});
        let got = resolve_single_dataset_id_from_args(&args).expect("ok");
        assert_eq!(got, None);
    }
}

fn ensure_dataset_comment(yaml_text: &str, dataset_id: &str) -> String {
    let wanted = format!("# Dataset: {}", dataset_id);
    // If already present (anywhere in the first 5 lines), keep original
    let lines: Vec<&str> = yaml_text.split('\n').collect();
    let scan_end = std::cmp::min(lines.len(), 5);
    for i in 0..scan_end {
        if lines[i].trim_start().starts_with("# Dataset:") {
            return yaml_text.to_string();
        }
    }
    // Insert comment at the very top followed by a blank line
    let mut out = String::new();
    out.push_str(&wanted);
    out.push('\n');
    out.push('\n');
    out.push_str(yaml_text);
    out
}

#[allow(dead_code)]
fn preprocess_model_sql(raw: &str) -> String {
    // Strip Jinja config and template blocks commonly used by dbt
    // Remove lines like "{{ config(...) }}" and unwrap {{ ref('x.y') }} -> x.y
    let mut out = String::new();
    let mut in_block_comment = false;
    for line in raw.lines() {
        let mut l = line.to_string();
        // Remove block comments /* ... */
        if in_block_comment {
            if let Some(end) = l.find("*/") {
                l = l[end + 2..].to_string();
                in_block_comment = false;
            } else {
                continue;
            }
        }
        if let Some(start) = l.find("/*") {
            if let Some(end) = l.find("*/") {
                l.replace_range(start..end + 2, "");
            } else {
                in_block_comment = true;
                l.replace_range(start.., "");
            }
        }
        let lt = l.trim();
        // Drop config-only lines
        if lt.starts_with("{{") && lt.contains("config(") {
            continue;
        }
        // Unwrap ref('x') or ref(\"x\") and source('db','table') → db.table
        let mut ll = l.clone();
        while let Some(start) = ll.find("{{") {
            if let Some(end) = ll[start..].find("}}") {
                let expr = &ll[start + 2..start + end].trim();
                let replacement = if expr.starts_with("ref(") {
                    let inner = expr.trim_start_matches("ref(").trim_end_matches(')').trim();
                    inner.trim_matches('"').trim_matches('\'').to_string()
                } else if expr.starts_with("source(") {
                    let inner = expr
                        .trim_start_matches("source(")
                        .trim_end_matches(')')
                        .trim();
                    let parts: Vec<&str> = inner
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').trim_matches('\''))
                        .collect();
                    if parts.len() == 2 {
                        format!("{}.{}", parts[0], parts[1])
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                ll.replace_range(start..start + end + 2, &replacement);
            } else {
                break;
            }
        }
        // Remove line comments
        let ll2 = if let Some(pos) = ll.find("--") {
            ll[..pos].to_string()
        } else {
            ll
        };
        out.push_str(&ll2);
        out.push('\n');
    }
    out.trim().to_string()
}
