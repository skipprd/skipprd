use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

use sha2::{Digest, Sha256};

use crate::data_engineer::dbt_repair::remediate::active_provider_dialect;
use crate::data_engineer::naming::{canonical_staging_model_name, contains_expected_source_call};
use crate::data_engineer::plan;
use crate::data_engineer::project_files;
use crate::data_engineer::project_fs;
use crate::data_engineer::sql_first;
use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_dataset_ids(args: &Value) -> Result<Vec<String>, String> {
    // Required shape: dataset_ids: [ ... ]
    let mut out: Vec<String> = args
        .get("dataset_ids")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err(
            "staging_model requires args.dataset_ids (string[]) where each item is <catalog>.<schema>.<table>. Refusing to default to all datasets."
                .to_string(),
        );
    }
    // Validate format early to avoid silent no-ops.
    for ds in out.iter() {
        if parse_dataset_id(ds).is_none() {
            return Err(format!(
                "invalid dataset_id '{ds}'. Expected <catalog>.<schema>.<table> (e.g. AwsDataCatalog.test_raw.raw_customers)."
            ));
        }
    }
    Ok(out)
}

fn resolve_instructions(args: &Value) -> String {
    // Prefer "instructions", but accept common aliases used in prompts/logs.
    extract_string_arg(args, "instructions")
        .or_else(|| extract_string_arg(args, "user_instructions"))
        .unwrap_or_default()
}

fn resolve_direct_sql(args: &Value) -> Option<String> {
    // Accept common keys used by agents/prompts.
    extract_string_arg(args, "sql").or_else(|| extract_string_arg(args, "expression"))
}

fn emit_trace(ctx: &AgentCtx, line: impl Into<String>) {
    if let Some(tx) = ctx.trace_tx.as_ref() {
        let _ = tx.send(line.into());
    }
}

fn athena_alias_reuse_hint(msg: &str, model_rel_path: &str, dataset_id: &str) -> Option<Value> {
    let m = msg.to_ascii_lowercase();
    if !m.contains("select-list alias") {
        return None;
    }
    Some(serde_json::json!({
        "kind": "athena_select_alias_reuse",
        "dataset_id": dataset_id,
        "model_path": model_rel_path,
        "issue": msg,
        "fix": "Split into CTE + outer select: compute intermediate aliases in an inner CTE/subquery, then reference them only from the outer SELECT."
    }))
}

#[derive(Clone)]
pub struct StagingModelTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

fn parse_dataset_id(dataset_id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = dataset_id.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

fn staging_model_rel_path_for_name(model_name: &str) -> String {
    format!("models/staging/{}.sql", model_name)
}

fn matching_staging_rel_paths_by_source(
    staging_files: &[(String, String)],
    expected_db: &str,
    expected_table: &str,
) -> Vec<String> {
    staging_files
        .iter()
        .filter_map(|(rel_path, content)| {
            if contains_expected_source_call(content, expected_db, expected_table) {
                Some(rel_path.clone())
            } else {
                None
            }
        })
        .collect()
}

fn build_staging_sys_prompt(
    provider: &str,
    dialect: &str,
    _expected_db: &str,
    _expected_table: &str,
    provider_rules: &str,
) -> String {
    format!(
        "You are an expert analytics engineer.\n\
         Task: write a SILVER model query as PLAIN SQL (no dbt config, no Jinja).\n\
         Provider: {provider}\n\
         Dialect: {dialect}\n\
         Requirements:\n\
         - Output MUST be valid JSON only: {{\"sql\":\"...\", \"notes\":[...]}}.\n\
         - Your SQL MUST read from the placeholder table name: FROM __SOURCE__\n\
           - Do NOT use source() / ref() / Jinja in this step.\n\
           - The system will replace __SOURCE__ with the real raw table for validation, then with dbt source() for materialization.\n\
         - This is SILVER: include sensible cleansing/normalization and stable column naming.\n\
         - IMPORTANT: The user payload may include plan invariants/notes; invariants are hard requirements.\n\
         - Dialect/provider compatibility:\n\
{provider_rules}\
         - Use the provided schema_columns types to guide casting and cleansing. Do NOT guess types from names.\n\
         - CRITICAL: Do NOT select or reference any column that is not present in schema_columns.\n\
           If the desired field is missing, note it and proceed with the closest available alternative.\n\
         - Time-like fields MUST be detected from schema types when possible:\n\
           - timestamp/datetime types: timestamp, timestamptz, datetime\n\
           - date types: date\n\
           If a field is string-typed but appears to encode time values, you may treat it as time-like only if schema_columns or samples strongly indicate it.\n\
         - IMPORTANT time handling (consistency):\n\
           - If schema_columns says a field is already a time type (timestamp/date/timestamptz/datetime), use it directly (quote the identifier if needed) and alias to the cleaned name.\n\
             - Do NOT re-cast typed timestamps (avoid noise like cast(ts as timestamp)).\n\
             - Do NOT narrow time zones: never cast timestamptz -> timestamp. Preserve the source type.\n\
             - Do NOT create *_raw helpers for already-typed time fields.\n\
           - Only use *_raw + try_cast parsing when the schema type is string-ish (varchar/string/text) or unknown and evidence indicates it encodes time.\n\
         - For any time-like field:\n\
           - If the source is string-ish: create a `*_raw` expression using trim + nullif-empty so empty strings become NULL deterministically.\n\
           - Produce the cleaned output field as a safe cast (Athena/Trino: try_cast(... as timestamp) or try_cast(... as date)).\n\
           - Row preservation (CRITICAL): do NOT filter rows to enforce non-nullness in silver.\n\
             - Keep NULLs and add quality flags (e.g. is_valid_*) where helpful.\n\
             - Recommend conditional dbt tests (with where:) only when raw input is present, and document why.\n\
           - IMPORTANT: Never recommend an unconditional not_null test on a try_cast-produced field; cast failures legitimately yield NULL.\n\
         - Silver must be row-preserving:\n\
           - Do NOT enforce grains/primary keys in silver (no deduping, no windowing row_number(), no filtering to non-null IDs).\n\
           - Do NOT add `*_pk` fields that imply enforced uniqueness; if you add canonical IDs, they must be nullable and accompanied by has_* flags.\n\
         - IMPORTANT: Do NOT include a dbt config block or alias; the suite enforces canonical config/alias deterministically.\n\
         - Nested fields / dotted columns:\n\
           - Use schema_columns as ground truth.\n\
           - If schema_columns contains an EXACT column name with dots (e.g. context.session.id), treat it as a literal column name and reference it as a single quoted identifier like \"context.session.id\".\n\
           - Only use struct dereference (e.g. context.session.id) when schema_columns indicates a struct/row parent exists (e.g. context) AND there is no exact dotted column name.\n\
         - If a column name is reserved (e.g. timestamp), quote the identifier (\"timestamp\"). For literal dotted column names, quote the entire identifier (\"context.session.id\").\n\
         - Keep changes aligned with the user's instructions, even if they are unconventional.\n"
        ,
        provider_rules = provider_rules
    )
}

fn render_plan_driven_instructions(
    invariants: &[String],
    checklist: &[crate::data_engineer::plan::PlanChecklistItem],
) -> String {
    let mut out = String::new();
    if !invariants.is_empty() {
        out.push_str("Plan invariants (MUST satisfy):\n");
        for inv in invariants.iter() {
            let t = inv.trim();
            if t.is_empty() {
                continue;
            }
            out.push_str("- ");
            out.push_str(t);
            out.push('\n');
        }
        out.push('\n');
    }
    let mut any = false;
    for it in checklist.iter() {
        let has_details = it
            .details
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let include = it.status != crate::data_engineer::plan::ChecklistItemStatus::Done || has_details;
        if !include {
            continue;
        }
        if !any {
            out.push_str("Plan checklist (remaining work):\n");
            any = true;
        }
        let origin = match it.origin {
            crate::data_engineer::plan::ChecklistOrigin::Initial => "initial",
            crate::data_engineer::plan::ChecklistOrigin::ReviewActionable => "review_actionable",
        };
        out.push_str("- ");
        out.push_str(it.label.trim());
        out.push_str(" (id=");
        out.push_str(it.checklist_item_id.trim());
        out.push_str(", status=");
        out.push_str(&format!("{:?}", it.status));
        out.push_str(", origin=");
        out.push_str(origin);
        out.push(')');
        if let Some(d) = it.details.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            out.push_str(": ");
            out.push_str(d);
        }
        out.push('\n');
    }
    if any {
        out.push('\n');
    }
    out.trim().to_string()
}

fn combine_instructions(user_instructions: &str, plan_instructions: &str) -> String {
    let ui = user_instructions.trim();
    let pi = plan_instructions.trim();
    if ui.is_empty() && pi.is_empty() {
        return String::new();
    }
    if ui.is_empty() {
        return pi.to_string();
    }
    if pi.is_empty() {
        return ui.to_string();
    }
    format!("User instructions:\n{}\n\n{}", ui, pi)
}

#[async_trait]
impl Tool for StagingModelTool {
    fn name(&self) -> &'static str {
        "staging_model"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let dbt = ctx
            .dbt
            .as_ref()
            .ok_or_else(|| "dbt provider missing".to_string())?;

        // Resolve datasets explicitly; NEVER default to all datasets.
        let mut dataset_ids = resolve_dataset_ids(&args)?;

        // Hard cap per call to keep LLM+patch loops bounded (prevents timeouts and huge prompts).
        // If more are provided, we process the first batch and return the rest as deferred so the
        // calling agent can issue subsequent staging_model calls.
        const MAX_DATASETS_PER_CALL: usize = 5;
        let deferred_dataset_ids: Vec<String> = if dataset_ids.len() > MAX_DATASETS_PER_CALL {
            dataset_ids.split_off(MAX_DATASETS_PER_CALL)
        } else {
            Vec::new()
        };

        let user_instructions = resolve_instructions(&args);

        // Plan-first authoring: if there is an active cleanse plan, use task invariants/notes as the
        // default authoring instructions (and merge with any explicit user override instructions).
        let plan_opt = plan::load_cleanse_plan(ctx).await;

        // Ensure minimal dbt project exists before writing artifacts.
        if let Err(e) = dbt.ensure_minimal_project(&ctx.scope).await {
            return Ok(serde_json::json!({
                "ok": false,
                "datasets": dataset_ids.len(),
                "written_keys": [],
                "schema_key": Value::Null,
                "notes": [],
                "errors": [format!("failed to ensure minimal dbt project: {e}")],
            }));
        }

        // Grounded gating (fail-fast, no side effects):
        // - Cleanse/silver must only operate on raw datasets (configured source_schema).
        // - The only fact we trust for dataset existence is query.schema(<fqn>) success.
        let cfg = crate::config::resolved_config_from_ctx(ctx).ok_or_else(|| {
            "resolved_config missing (cannot enforce raw dataset constraints)".to_string()
        })?;
        let want_catalog = cfg.providers.warehouse.container.clone();
        let want_schema = cfg.providers.warehouse.namespace.clone();
        let mut schema_cols_by_ds: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();
        let mut gating_errors: Vec<String> = Vec::new();
        for ds in dataset_ids.iter() {
            let Some((cat, db, _tbl)) = parse_dataset_id(ds) else {
                gating_errors.push(format!(
                    "{}: invalid dataset_id (expected <catalog>.<schema>.<table>)",
                    ds
                ));
                continue;
            };
            if cat != want_catalog {
                gating_errors.push(format!(
                    "{}: dataset is not in configured target catalog (expected {})",
                    ds, want_catalog
                ));
                continue;
            }
            if db != want_schema {
                gating_errors.push(format!(
                    "{}: cleanse/silver can only target raw datasets in schema '{}' (got '{}')",
                    ds, want_schema, db
                ));
                continue;
            }
            match ctx.warehouse.schema(ds).await {
                Ok(cols) => {
                    schema_cols_by_ds.insert(ds.clone(), cols);
                }
                Err(e) => {
                    gating_errors.push(format!(
                        "{}: schema lookup failed (treating as fact): {}",
                        ds, e
                    ));
                }
            }
        }
        if !gating_errors.is_empty() {
            // IMPORTANT: do not write schema.yml or any models if the dataset facts are not proven.
            return Ok(serde_json::json!({
                "ok": false,
                "datasets": dataset_ids.len(),
                "written_keys": [],
                "schema_key": Value::Null,
                "notes": [],
                "errors": gating_errors,
                "deferred_dataset_ids": deferred_dataset_ids,
                "succeeded_dataset_ids": [],
            }));
        }

        // Ensure models/schema.yml exists. We only "seed" it when missing.
        // For existing schema.yml, avoid applying no-op patches (diff parsers may reject header-only diffs).
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string();
        let schema_rel = project_files::MODELS_SCHEMA_YML.to_string();
        let schema_key = format!("{}/{}", base, schema_rel);
        let existing_schema: Option<String> = match ctx.storage.get_bytes(&schema_key).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
            Err(_) => None,
        };
        if existing_schema.is_none() {
            let seed = "version: 2\n".to_string();
            let outcome = match project_fs::apply_patch(
                ctx,
                self.datasets.as_ref(),
                &schema_rel,
                &seed,
                None,
                None,
                project_fs::PatchApplyKind::FullOverwrite,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "datasets": dataset_ids.len(),
                        "written_keys": [],
                        "schema_key": schema_key,
                        "notes": [],
                        "errors": [format!("failed to patch models/schema.yml: {e}")],
                    }));
                }
            };
            if let Err(e) = ctx
                .storage
                .put_bytes(&schema_key, outcome.content.as_bytes(), "text/yaml")
                .await
            {
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": [],
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": [format!("failed to write models/schema.yml: {e}")],
                }));
            }
        } else if let Some(existing) = existing_schema.as_deref() {
            // Canonicalize schema.yml deterministically and only apply/write if it actually changes.
            let canonical =
                match project_fs::canonicalize_schema_yml(ctx, self.datasets.as_ref(), existing)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "datasets": dataset_ids.len(),
                            "written_keys": [],
                            "schema_key": schema_key,
                            "notes": [],
                            "errors": [format!("failed to canonicalize models/schema.yml: {e}")],
                        }));
                    }
                };
            if canonical != existing {
                let outcome = match project_fs::apply_patch(
                    ctx,
                    self.datasets.as_ref(),
                    &schema_rel,
                    &canonical,
                    None,
                    None,
                    project_fs::PatchApplyKind::FullOverwrite,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "datasets": dataset_ids.len(),
                            "written_keys": [],
                            "schema_key": schema_key,
                            "notes": [],
                            "errors": [format!("failed to patch models/schema.yml: {e}")],
                        }));
                    }
                };
                if let Err(e) = ctx
                    .storage
                    .put_bytes(&schema_key, outcome.content.as_bytes(), "text/yaml")
                    .await
                {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "datasets": dataset_ids.len(),
                        "written_keys": [],
                        "schema_key": schema_key,
                        "notes": [],
                        "errors": [format!("failed to write models/schema.yml: {e}")],
                    }));
                }
            }
        }

        let dialect = crate::config::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());
        let provider_name = crate::config::resolved_config_from_ctx(ctx)
            .map(|cfg| cfg.providers.warehouse.kind.as_str())
            .unwrap_or("unknown");
        let provider_prompt_rules = {
            let mut out = String::new();
            for rule in ctx.warehouse.sql_prompt_rules().into_iter() {
                out.push_str("           - ");
                out.push_str(rule);
                out.push('\n');
            }
            out
        };

        // Discover existing staging model files so we can update by semantic identity (source()),
        // not by filename (prevents duplicate staging models for the same dataset).
        let staging_prefix = format!("{}/models/staging/", base);
        let mut staging_files: Vec<(String, String)> = Vec::new(); // (rel_path, content)
        let mut unreadable_staging_rel_paths: Vec<String> = Vec::new();
        if let Ok(keys) = ctx.storage.list_prefix(&staging_prefix).await {
            for k in keys {
                if !k.ends_with(".sql") {
                    continue;
                }
                if k.contains("/_versions/") {
                    continue;
                }
                // Convert storage key -> project-relative path (models/staging/<name>.sql)
                let rel_path = k
                    .strip_prefix(&(base.clone() + "/"))
                    .unwrap_or(k.as_str())
                    .to_string();
                match ctx.storage.get_bytes(&k).await {
                    Ok(bytes) => {
                        let content = String::from_utf8_lossy(&bytes).to_string();
                        staging_files.push((rel_path, content));
                    }
                    Err(_) => {
                        unreadable_staging_rel_paths.push(rel_path);
                    }
                }
            }
        }

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut remediation_hints: Vec<Value> = Vec::new();
        let mut succeeded_dataset_ids: Vec<String> = Vec::new();

        if !deferred_dataset_ids.is_empty() {
            notes.push(format!(
                "staging_model processes at most {MAX_DATASETS_PER_CALL} dataset_ids per call; deferred {} dataset(s) for a follow-up call.",
                deferred_dataset_ids.len()
            ));
        }

        // Optional fast-path: direct write mode. If SQL is provided, we write exactly one dataset's model
        // without calling the LLM (avoids extra OpenAI calls and reduces timeout risk).
        if let Some(sql_out) = resolve_direct_sql(&args) {
            if dataset_ids.len() != 1 {
                return Err("staging_model direct-write requires exactly one dataset (use a single-item args.dataset_ids).".to_string());
            }
            let ds = &dataset_ids[0];
            let (_cat, expected_db, expected_table) = parse_dataset_id(ds).ok_or_else(|| {
                format!("invalid dataset_id '{ds}' (expected <catalog>.<schema>.<table>)")
            })?;
            let canonical_name = canonical_staging_model_name(&expected_db, &expected_table);

            let matches =
                matching_staging_rel_paths_by_source(&staging_files, &expected_db, &expected_table);
            if matches.len() > 1 {
                errors.push(format!(
                    "{ds}: multiple staging models reference the same source({expected_db},{expected_table}); refusing to write. Conflicts: {:?}",
                    matches
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            if matches.is_empty() && !unreadable_staging_rel_paths.is_empty() {
                errors.push(format!(
                    "{ds}: unable to reliably determine whether an existing staging model already targets this source because some staging files were unreadable. Unreadable: {:?}",
                    unreadable_staging_rel_paths
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            let rel_path = matches
                .get(0)
                .cloned()
                .unwrap_or_else(|| staging_model_rel_path_for_name(&canonical_name));
            let key = format!("{}/{}", base, rel_path);

            let has_any_source = sql_out.to_lowercase().contains("source(");
            if !has_any_source {
                errors.push(format!(
                    "staging_model requires using a dbt source(). Expected to read from: {{ source(\"{expected_db}\", \"{expected_table}\") }} (derived from dataset_id {ds})."
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            if !contains_expected_source_call(&sql_out, &expected_db, &expected_table) {
                errors.push(format!(
                    "staging_model produced a source() call that does not match the expected source/table for dataset_id {ds}. Expected: {{ source(\"{expected_db}\", \"{expected_table}\") }}."
                ));
                return Ok(serde_json::json!({
                    "ok": false,
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            let existing_opt = ctx
                .storage
                .get_bytes(&key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string());
            let _existed = existing_opt.is_some();
            let _existing = existing_opt.unwrap_or_default();
            let outcome = project_fs::apply_patch(
                ctx,
                None,
                &rel_path,
                &sql_out,
                None,
                None,
                project_fs::PatchApplyKind::FullOverwrite,
            )
            .await?;
            if let Err(e) = ctx
                .storage
                .put_bytes(&key, outcome.content.as_bytes(), "text/sql")
                .await
            {
                emit_trace(ctx, format!("failed to save {}: {}", rel_path, e));
                return Err(e.to_string());
            }
            emit_trace(ctx, format!("saved {}", rel_path));
            written.push(key);
            succeeded_dataset_ids.push(ds.clone());

            info!(
                target: "staging_model",
                datasets = dataset_ids.len(),
                written = written.len(),
                "staging_model finished (direct-write)"
            );

            return Ok(serde_json::json!({
                "ok": true,
                "datasets": dataset_ids.len(),
                "written_keys": written,
                "schema_key": schema_key,
                "notes": [],
                "deferred_dataset_ids": deferred_dataset_ids,
                "succeeded_dataset_ids": succeeded_dataset_ids,
            }));
        }

        for ds in dataset_ids.iter() {
            let (_cat, expected_db, expected_table) = parse_dataset_id(ds).ok_or_else(|| {
                format!("invalid dataset_id '{ds}' (expected <catalog>.<schema>.<table>)")
            })?;
            let canonical_name = canonical_staging_model_name(&expected_db, &expected_table);

            let matches =
                matching_staging_rel_paths_by_source(&staging_files, &expected_db, &expected_table);
            if matches.len() > 1 {
                errors.push(format!(
                    "{ds}: multiple staging models reference the same source({expected_db},{expected_table}); refusing to write. Conflicts: {:?}",
                    matches
                ));
                continue;
            }
            if matches.is_empty() && !unreadable_staging_rel_paths.is_empty() {
                errors.push(format!(
                    "{ds}: unable to reliably determine whether an existing staging model already targets this source because some staging files were unreadable. Unreadable: {:?}",
                    unreadable_staging_rel_paths
                ));
                continue;
            }

            // Exactly 1 match -> update in place (even if filename isn't canonical).
            // No matches -> create at canonical path.
            let rel_path = matches
                .get(0)
                .cloned()
                .unwrap_or_else(|| staging_model_rel_path_for_name(&canonical_name));
            let key = format!("{}/{}", base, rel_path);

            // Use the pre-validated schema facts (guaranteed present by gating above).
            let cols = schema_cols_by_ds.get(ds).cloned().unwrap_or_default();
            let cols_json: Vec<Value> = cols
                .iter()
                .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                .collect();

            let sys = build_staging_sys_prompt(
                provider_name,
                &dialect,
                &expected_db,
                &expected_table,
                &provider_prompt_rules,
            );

            let existing_sql = ctx
                .storage
                .get_bytes(&key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();

            let (plan_invariants, plan_checklist, plan_expected_model_path, plan_implementation_spec) = plan_opt
                .as_ref()
                .and_then(|p| p.tasks.iter().find(|t| t.dataset_id == *ds))
                .map(|t| {
                    (
                        t.invariants.clone(),
                        t.checklist.clone(),
                        t.expected_model_path.clone().unwrap_or_default(),
                        Some(t.implementation_spec.clone()),
                    )
                })
                .unwrap_or_else(|| (vec![], vec![], String::new(), None));
            let plan_instr = render_plan_driven_instructions(&plan_invariants, &plan_checklist);
            let effective_instructions = combine_instructions(&user_instructions, &plan_instr);

            let cols_for_sql: Vec<String> = cols
                .iter()
                .map(|(n, _t)| ctx.warehouse.quote_ident(n))
                .collect();

            let user_value = serde_json::json!({
                "dataset_id": ds,
                "schema_columns": cols_json,
                "user_instructions": effective_instructions,
                "plan_invariants": plan_invariants,
                "plan_checklist": plan_checklist,
                "plan_implementation_spec": plan_implementation_spec,
                "plan_expected_model_path": plan_expected_model_path,
                "expected_model_path": rel_path,
                "existing_model_sql": existing_sql,
                "sql_first": {
                    "source_placeholder": "__SOURCE__",
                    "raw_dataset_fqn": ds,
                    "materialize_source_macro": format!("{{{{ source(\"{}\", \"{}\") }}}}", expected_db, expected_table),
                    "goal": "Return plain SQL that is safe + row-preserving. Keep output bounded; prefer explicit select list.",
                }
            });

            // SQL-first: draft plain SQL, validate against warehouse, then materialize to dbt SQL.
            let sys0 = sys;
            let max_tokens = sql_first::sql_first_max_output_tokens(6000);
            let max_attempts = sql_first::sql_first_max_repair_attempts(4);
            let mut repl = std::collections::HashMap::new();
            let dsid = match ctx.warehouse.parse_dataset_fqn(ds) {
                Ok(id) => id,
                Err(e) => {
                    errors.push(format!("{ds}: invalid dataset fqn: {e}"));
                    continue;
                }
            };
            repl.insert("__SOURCE__".to_string(), ctx.warehouse.quote_fqn(&dsid));

            let mut last_err = String::new();
            let mut prev_sql: Option<String> = None;
            let mut draft: Option<sql_first::SqlFirstDraft> = None;
            let mut ok = false;
            for attempt in 1..=max_attempts {
                let mut v = user_value.clone();
                if attempt > 1 {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("attempt".to_string(), serde_json::json!(attempt));
                        obj.insert(
                            "previous_sql".to_string(),
                            serde_json::json!(prev_sql.clone().unwrap_or_default()),
                        );
                        obj.insert("error".to_string(), serde_json::json!(last_err.clone()));
                        obj.insert("instruction".to_string(), serde_json::json!("Fix the SQL. Output JSON only: {\"sql\":\"...\",\"notes\":[...]}. Must use FROM __SOURCE__. Must be row-preserving (do not filter rows; avoid joins that can multiply rows). Must not reference columns outside schema_columns. Prefer explicit select list; avoid SELECT *."));
                    }
                }

                let sys_msg = if attempt == 1 {
                    sys0.clone()
                } else {
                    build_staging_sys_prompt(
                        provider_name,
                        &dialect,
                        &expected_db,
                        &expected_table,
                        &provider_prompt_rules,
                    )
                };
                let prompt_id = if attempt == 1 {
                    "data_engineer.tools.staging_model.sql_first"
                } else {
                    "data_engineer.tools.staging_model.sql_first_repair"
                };
                let temp = if attempt == 1 { 0.08 } else { 0.05 };
                let user_json = v.to_string();

                let mut d = match sql_first::llm_draft_sql_json(
                    ctx,
                    sys_msg,
                    user_json,
                    prompt_id,
                    max_tokens,
                    temp,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = e;
                        if attempt >= max_attempts {
                            errors.push(format!("{ds}: sql draft failed: {last_err}"));
                        }
                        continue;
                    }
                };

                if !d.sql.contains("__SOURCE__") {
                    last_err = "draft SQL must reference __SOURCE__ placeholder".to_string();
                    prev_sql = Some(d.sql.clone());
                    if attempt >= max_attempts {
                        errors.push(format!("{ds}: sql draft invalid: {last_err}"));
                    }
                    continue;
                }

                // Deterministic SELECT * expansion (simple passthrough only).
                if let Some(s) =
                    sql_first::expand_select_star_from_placeholder(&d.sql, "__SOURCE__", &cols_for_sql)
                {
                    d.sql = s;
                }

                match sql_first::validate_sql_quick(ctx, &d.sql, &repl).await {
                    Ok(()) => {
                        draft = Some(d);
                        ok = true;
                        break;
                    }
                    Err(err) => {
                        last_err = err.clone();
                        prev_sql = Some(d.sql.clone());
                        if let Some(h) = athena_alias_reuse_hint(&err, &rel_path, ds) {
                            remediation_hints.push(h);
                        }
                        if attempt >= max_attempts {
                            errors.push(format!("{ds}: sql validation failed: {err}"));
                        }
                        continue;
                    }
                }
            }
            if !ok {
                continue;
            }
            let draft = draft.expect("ok implies draft");

            // Materialize: replace __SOURCE__ with dbt source().
            let dbt_sql = draft
                .sql
                .replace("__SOURCE__", &format!("{{{{ source(\"{}\", \"{}\") }}}}", expected_db, expected_table));
            if !contains_expected_source_call(&dbt_sql, &expected_db, &expected_table) {
                errors.push(format!(
                    "{ds}: materialized sql missing expected source(\"{expected_db}\",\"{expected_table}\")"
                ));
                continue;
            }
            let base_sha256 = if existing_sql.is_empty() {
                None
            } else {
                Some(sha256_hex(&existing_sql))
            };
            let outcome = match project_fs::apply_patch(
                ctx,
                None,
                &rel_path,
                &dbt_sql,
                base_sha256.as_deref(),
                Some(!existing_sql.is_empty()),
                project_fs::PatchApplyKind::FullOverwrite,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    errors.push(format!("{ds}: materialize apply_patch failed: {e}"));
                    continue;
                }
            };
            if let Err(e) = ctx
                .storage
                .put_bytes(&key, outcome.content.as_bytes(), "text/sql")
                .await
            {
                emit_trace(ctx, format!("failed to save {}: {}", rel_path, e));
                errors.push(format!("{ds}: failed to write silver model: {e}"));
                continue;
            }
            emit_trace(ctx, format!("saved {}", rel_path));
            written.push(key);
            succeeded_dataset_ids.push(ds.clone());
            for n in draft.notes {
                if !n.trim().is_empty() {
                    notes.push(format!("{}: {}", ds, n));
                }
            }
        }

        // Minimal, not chatty
        info!(
            target: "staging_model",
            datasets = dataset_ids.len(),
            written = written.len(),
            "staging_model finished"
        );

        // Dedup notes to keep response bounded
        let mut seen: HashSet<String> = HashSet::new();
        let mut out_notes: Vec<String> = Vec::new();
        for n in notes {
            if seen.insert(n.clone()) {
                out_notes.push(n);
            }
            if out_notes.len() >= 50 {
                break;
            }
        }

        Ok(serde_json::json!({
            "ok": errors.is_empty(),
            "datasets": dataset_ids.len(),
            "written_keys": written,
            "schema_key": schema_key,
            "notes": out_notes,
            "remediation_hints": remediation_hints,
            "errors": errors,
            "deferred_dataset_ids": deferred_dataset_ids,
            "succeeded_dataset_ids": succeeded_dataset_ids,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_engineer::naming::extract_source_calls;
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::providers::{DbtProvider, QueryProvider, QueryResult};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

    #[test]
    fn resolve_dataset_ids_accepts_dataset_ids_array() {
        let args = serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders","AwsDataCatalog.test_raw.raw_orders"]});
        let got = resolve_dataset_ids(&args).expect("ok");
        assert_eq!(got, vec!["AwsDataCatalog.test_raw.raw_orders".to_string()]);
    }

    #[test]
    fn resolve_dataset_ids_rejects_missing() {
        let args = serde_json::json!({});
        let err = resolve_dataset_ids(&args).unwrap_err();
        assert!(err.contains("requires args.dataset_ids"));
        assert!(err.contains("Refusing to default"));
    }

    #[test]
    fn resolve_dataset_ids_rejects_invalid_format() {
        let args = serde_json::json!({"dataset_ids":["test_raw__raw_customers"]});
        let err = resolve_dataset_ids(&args).unwrap_err();
        assert!(err.contains("invalid dataset_id"));
    }

    #[test]
    fn resolve_direct_sql_prefers_sql_then_expression() {
        let a = serde_json::json!({"sql":"select 1"});
        assert_eq!(resolve_direct_sql(&a).as_deref(), Some("select 1"));

        let b = serde_json::json!({"expression":"select 2"});
        assert_eq!(resolve_direct_sql(&b).as_deref(), Some("select 2"));
    }

    #[test]
    fn contains_expected_source_call_accepts_common_formats() {
        let sql1 = "select * from {{ source('test_raw','raw_orders') }}";
        let sql2 = "select * from {{ source(\"test_raw\", \"raw_orders\") }}";
        assert!(contains_expected_source_call(
            sql1,
            "test_raw",
            "raw_orders"
        ));
        assert!(contains_expected_source_call(
            sql2,
            "test_raw",
            "raw_orders"
        ));
        assert!(!contains_expected_source_call(
            sql2,
            "test_raw",
            "raw_customers"
        ));
    }

    #[test]
    fn canonical_staging_model_name_is_deterministic_schema_table() {
        let got = canonical_staging_model_name("test_raw", "raw_orders");
        assert_eq!(got, "stg_test_raw_raw_orders");
        assert!(!got.contains("__"));
    }

    #[test]
    fn staging_sys_prompt_does_not_force_struct_dereference() {
        let sys = build_staging_sys_prompt(
            "athena",
            "Amazon Athena (engine v3 / Trino SQL)",
            "picnic",
            "track_app_opened",
            "           - If Provider is athena (Trino SQL), DO NOT use initcap() (it is not registered). Avoid title-casing strings.\n",
        );
        assert!(!sys.contains("DO NOT quote the whole path"));
        assert!(sys.contains("schema_columns as ground truth"));
        assert!(sys.contains("quote the entire identifier"));
    }

    #[test]
    fn staging_sys_prompt_includes_bigquery_alias_scope_rule() {
        let sys = build_staging_sys_prompt(
            "bigquery",
            "Google BigQuery (Standard SQL)",
            "newyork",
            "collisions",
            "           - If Provider is bigquery (Google BigQuery Standard SQL), never reference a SELECT-list alias inside another expression in the same SELECT list. If one derived field depends on another, split into CTE/subquery + outer SELECT.\n           - If Provider is bigquery, use SAFE_CAST(...) for tolerant casts (not try_cast).\n",
        );
        assert!(sys.contains("never reference a SELECT-list alias"));
        assert!(sys.contains("SAFE_CAST"));
    }

    #[test]
    fn matching_staging_rel_paths_by_source_finds_exactly_one() {
        let files = vec![
            (
                "models/staging/stg_x.sql".to_string(),
                "select * from {{ source('test_raw','raw_orders') }}".to_string(),
            ),
            (
                "models/staging/stg_y.sql".to_string(),
                "select 1".to_string(),
            ),
        ];
        let got = matching_staging_rel_paths_by_source(&files, "test_raw", "raw_orders");
        assert_eq!(got, vec!["models/staging/stg_x.sql".to_string()]);
    }

    #[test]
    fn extract_source_calls_finds_schema_table() {
        let sql = "select * from {{ source('TeSt_Raw', 'Raw_Orders') }}";
        let got = extract_source_calls(sql);
        assert_eq!(
            got,
            vec![("test_raw".to_string(), "raw_orders".to_string())]
        );
    }

    #[test]
    fn matching_staging_rel_paths_by_source_detects_duplicates() {
        let files = vec![
            (
                "models/staging/a.sql".to_string(),
                "select * from {{ source('test_raw','raw_orders') }}".to_string(),
            ),
            (
                "models/staging/b.sql".to_string(),
                "select * from {{ source(\"test_raw\", \"raw_orders\") }}".to_string(),
            ),
        ];
        let got = matching_staging_rel_paths_by_source(&files, "test_raw", "raw_orders");
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn staging_model_fails_fast_when_schema_facts_do_not_prove_dataset() {
        #[derive(Clone)]
        struct MockDbt;
        #[async_trait]
        impl DbtProvider for MockDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> {
                Ok(())
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _dataset_id: &str,
                _name: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _dataset_id: &str,
                _name: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &react_core::providers::DbtValidateArgs,
            ) -> Result<react_core::providers::DbtValidateResult, String> {
                Err("not used".to_string())
            }
        }

        #[derive(Clone)]
        struct MockQuery;
        #[async_trait]
        impl QueryProvider for MockQuery {
            async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
                Err("not used".to_string())
            }
            async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
                Err(format!("not found: {}", dataset_fqn))
            }
            async fn sample(
                &self,
                _dataset_fqn: &str,
                _limit: usize,
            ) -> Result<Vec<Vec<String>>, String> {
                Err("not used".to_string())
            }
        }

        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let cfg = Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: scope.clone(),
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        });

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: Some("t1".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: Arc::new(react_core::llm::NullModel::new()),
            storage: storage.clone(),
            scope,
            keyspace,
            query: Some(Arc::new(MockQuery)),
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: Some(Arc::new(MockDbt)),
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(cfg as Arc<dyn std::any::Any + Send + Sync>),
        };

        let tool = StagingModelTool { datasets: None };
        let obs = tool
            .call(
                serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_customers"]}),
                &ctx,
            )
            .await
            .expect("call ok");

        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            obs.get("written_keys")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );

        // Ensure we didn't write schema.yml as a side effect.
        let schema_key = format!(
            "{}{}",
            ctx.keyspace.dbt_prefix(&ctx.scope),
            crate::data_engineer::project_files::MODELS_SCHEMA_YML
        );
        assert!(storage.get_bytes(&schema_key).await.is_err());
    }

    #[tokio::test]
    async fn staging_model_uses_cleanse_plan_invariants_and_notes_as_default_instructions() {
        #[derive(Clone)]
        struct MockDbt;
        #[async_trait]
        impl DbtProvider for MockDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> {
                Ok(())
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _dataset_id: &str,
                _name: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _dataset_id: &str,
                _name: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &react_core::providers::DbtValidateArgs,
            ) -> Result<react_core::providers::DbtValidateResult, String> {
                Err("not used".to_string())
            }
        }

        #[derive(Clone)]
        struct MockWarehouse;
        #[async_trait]
        impl QueryProvider for MockWarehouse {
            async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
                Ok(QueryResult {
                    header: vec![],
                    rows: vec![],
                    meta: None,
                })
            }
            async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
                if dataset_fqn == "AwsDataCatalog.test_raw.raw_orders" {
                    Ok(vec![
                        ("order_id".to_string(), "string".to_string()),
                        ("created_at".to_string(), "timestamp".to_string()),
                    ])
                } else {
                    Err(format!("not found: {}", dataset_fqn))
                }
            }
            async fn sample(
                &self,
                _dataset_fqn: &str,
                _limit: usize,
            ) -> Result<Vec<Vec<String>>, String> {
                Err("not used".to_string())
            }
        }
        #[async_trait]
        impl react_core::providers::DatasetCatalogProvider for MockWarehouse {
            async fn list_datasets(&self) -> Result<Vec<react_core::providers::DatasetId>, String> {
                Ok(vec![react_core::providers::DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_orders".to_string(),
                }])
            }

            async fn get_dataset_schema(
                &self,
                dataset: &react_core::providers::DatasetId,
            ) -> Result<Vec<(String, String)>, String> {
                self.schema(&dataset.fqn()).await
            }

            async fn get_dataset_stats(
                &self,
                _dataset: &react_core::providers::DatasetId,
                _max_fields: usize,
            ) -> Result<
                (
                    react_core::discover::stats::DatasetFieldStats,
                    react_core::providers::catalog::types::DatasetStats,
                ),
                String,
            > {
                Err("not used".to_string())
            }
        }
        impl react_core::providers::WarehouseNaming for MockWarehouse {
            fn kind(&self) -> &'static str {
                "mock"
            }
            fn parse_dataset_fqn(
                &self,
                fqn: &str,
            ) -> Result<react_core::providers::DatasetId, String> {
                let parts: Vec<&str> = fqn.split('.').collect();
                if parts.len() != 3 {
                    return Err("expected <catalog>.<schema>.<table>".to_string());
                }
                Ok(react_core::providers::DatasetId {
                    catalog: parts[0].to_string(),
                    database: parts[1].to_string(),
                    table: parts[2].to_string(),
                })
            }
            fn quote_ident(&self, ident: &str) -> String {
                format!("\"{}\"", ident.replace('"', "\"\""))
            }
        }

        #[derive(Clone)]
        struct CapturingLlm {
            resp: String,
            captured_user_instructions: Arc<Mutex<Option<String>>>,
        }
        impl LargeLanguageModel for CapturingLlm {
            fn chat(
                &self,
                messages: &[ChatMessage],
                _options: &react_core::llm::LlmCallOptions,
            ) -> Result<String, String> {
                let user = messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if let Ok(v) = serde_json::from_str::<Value>(&user) {
                    let instr = v
                        .get("user_instructions")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                    if let Ok(mut g) = self.captured_user_instructions.lock() {
                        *g = instr;
                    }
                }
                Ok(self.resp.clone())
            }
            fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
                Ok(vec![])
            }
        }

        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let cfg = Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: scope.clone(),
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        });

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let llm = Arc::new(CapturingLlm {
            resp: serde_json::json!({
                "sql": "select order_id, created_at from __SOURCE__",
                "notes": []
            })
            .to_string(),
            captured_user_instructions: captured.clone(),
        });

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: Some("t1".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(MockWarehouse),
            dbt: Some(Arc::new(MockDbt)),
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(cfg as Arc<dyn std::any::Any + Send + Sync>),
        };

        // Seed an approved cleanse plan with invariants/checklist for this dataset.
        let ds = "AwsDataCatalog.test_raw.raw_orders".to_string();
        let plan_key = crate::data_engineer::plan::new_cleanse_plan_key(&ctx);
        let plan = crate::data_engineer::plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: crate::data_engineer::plan::PlanStatus::Approved,
            project_snapshot: Value::Null,
            tasks: vec![crate::data_engineer::plan::CleanseTask {
                dataset_id: ds.clone(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec!["Staging grain: exactly 1 row per order_pk.".to_string()],
                implementation_spec: crate::data_engineer::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![crate::data_engineer::plan::OutputFieldSpec {
                        name: "order_id_raw".to_string(),
                        kind: crate::data_engineer::plan::FieldKind::Raw,
                        source_columns: vec!["order_id".to_string()],
                        expression: "order_id as order_id_raw (raw passthrough)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![
                        "filtering".to_string(),
                        "deduplication".to_string(),
                        "grain_enforcement".to_string(),
                    ],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: vec![crate::data_engineer::plan::PlanChecklistItem {
                    checklist_item_id: "sql_model".to_string(),
                    label: "Author staging SQL".to_string(),
                    details: Some("Add a canonical order_pk and document behavior.".to_string()),
                    status: crate::data_engineer::plan::ChecklistItemStatus::Pending,
                    origin: crate::data_engineer::plan::ChecklistOrigin::Initial,
                    origin_step_idx: None,
                    evidence: vec![],
                }],
            }],
            batches: vec![vec![ds.clone()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_cleanse_plan(&ctx, &plan)
            .await
            .unwrap();

        let tool = StagingModelTool { datasets: None };
        let out = tool
            .call(serde_json::json!({"dataset_ids":[ds]}), &ctx)
            .await
            .expect("tool call");

        assert!(out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
        let got = captured
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        assert!(got.contains("Plan invariants"));
        assert!(got.contains("exactly 1 row"));
        assert!(got.contains("Plan checklist"));
        assert!(got.contains("canonical order_pk"));

        // Avoid unused var warning in case future refactors remove ctx mutability.
        assert!(!plan_key.trim().is_empty());
    }
}
