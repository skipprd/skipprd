use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tracing::info;

use crate::dialect::active_provider_dialect;
use crate::failure_kind::FailureKind;
use crate::naming::{canonical_staging_model_name, contains_expected_source_call};
use crate::plan;
use crate::project_fs;
use crate::providers::DatasetCatalogProvider;
use crate::references::DatasetRef;
use crate::sql_first;
use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix, retry_put_bytes};
use react_core::tools::Tool;

use super::model_authoring_engine::{
    self as engine, build_provider_prompt_rules, dedup_notes, emit_trace, extract_string_arg,
};

fn resolve_dataset_ids(args: &Value) -> Result<Vec<DatasetRef>, String> {
    // Required shape: dataset_ids: [ ... ]
    let raw_ids: Vec<String> = args
        .get("dataset_ids")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut out: Vec<DatasetRef> = Vec::new();
    for ds in raw_ids {
        let parsed = DatasetRef::parse(&ds).ok_or_else(|| {
            format!(
                "invalid dataset_id '{ds}'. Expected <catalog>.<schema>.<table> (e.g. AwsDataCatalog.test_raw.raw_customers)."
            )
        })?;
        out.push(parsed);
    }

    out.sort_by_key(|d| d.fqn());
    out.dedup();
    if out.is_empty() {
        return Err(
            "staging_model requires args.dataset_ids (string[]) where each item is <catalog>.<schema>.<table>. Refusing to default to all datasets."
                .to_string(),
        );
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

#[derive(Clone)]
pub struct StagingModelTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
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
         - Your SQL MUST read from the placeholder table name: FROM __SOURCE__\n\
           - Do NOT use source() / ref() / Jinja in this step.\n\
           - The system will replace __SOURCE__ with the real raw table for validation, then with dbt source() for materialization.\n\
         - This is SILVER: include sensible cleansing/normalization and stable column naming.\n\
         - IMPORTANT: The user payload may include plan invariants/notes; invariants are hard requirements.\n\
         - Dialect/provider compatibility:\n\
{provider_rules}\
         - Use the provided schema_columns types to guide casting and cleansing. Do NOT guess types from names.\n\
         - If plan_implementation_spec is present, it is the approved published schema contract:\n\
           - The final SELECT MUST output exactly plan_implementation_spec.output_fields, no more and no fewer.\n\
           - Do NOT add *_raw passthrough columns unless they are explicitly listed in output_fields.\n\
           - output_fields expressions override general heuristics below, including time handling.\n\
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
         - ROW-PRESERVING COLUMN CONTRACT:\n\
           - Row-preserving means preserving row count and lineage, not publishing every raw source column.\n\
           - When plan_implementation_spec is present, publish only output_fields; use raw source columns inside CTEs as needed.\n\
           - When no plan_implementation_spec is present, include all columns from schema_columns with stable clean names.\n\
         - Prefer an explicit column list in the final SELECT; avoid SELECT *. If you use CTEs, expand the final projection rather than using SELECT * FROM cte.\n\
         - IMPORTANT: Do NOT include a dbt config block or alias; the suite enforces canonical config/alias deterministically.\n\
         - Nested fields / dotted columns:\n\
           - Use schema_columns as ground truth.\n\
           - If schema_columns contains a column name with dots (e.g. context.session.id), it represents a nested struct path. \
Reference it using unquoted struct dereference syntax (e.g. context.session.id) or per-segment quoting (e.g. \"context\".\"session\".\"id\"). \
Do NOT quote the entire dot-path as a single identifier — \"context.session.id\" will FAIL with COLUMN_NOT_FOUND.\n\
           - Inside CTEs you may alias dotted source fields to flat intermediate names for readability.\n\
           - The final SELECT column aliases MUST match plan_implementation_spec.output_fields[].name EXACTLY (the published contract). Do not flatten or rename outputs in the final projection unless output_fields declares that published name.\n\
         - If a column name is reserved (e.g. timestamp), quote the identifier (\"timestamp\").\n\
         - Keep changes aligned with the user's instructions, even if they are unconventional.\n"
        ,
        provider_rules = provider_rules
    )
}

use super::plan_prompt_helpers::{combine_instructions, render_plan_driven_instructions};

#[async_trait]
impl Tool for StagingModelTool {
    fn name(&self) -> &'static str {
        "staging_model"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let dbt =
            crate::ctx_ext::actx_dbt(ctx).ok_or_else(|| "dbt provider missing".to_string())?;

        // Resolve datasets explicitly; NEVER default to all datasets.
        let mut dataset_refs = resolve_dataset_ids(&args)?;

        // Hard cap per call to keep LLM+patch loops bounded (prevents timeouts and huge prompts).
        // If more are provided, we process the first batch and return the rest as deferred so the
        // calling agent can issue subsequent staging_model calls.
        const MAX_DATASETS_PER_CALL: usize = 5;
        let deferred_dataset_ids: Vec<String> = if dataset_refs.len() > MAX_DATASETS_PER_CALL {
            dataset_refs
                .split_off(MAX_DATASETS_PER_CALL)
                .into_iter()
                .map(|d| d.fqn())
                .collect()
        } else {
            Vec::new()
        };
        let dataset_ids: Vec<String> = dataset_refs.iter().map(|d| d.fqn()).collect();

        let user_instructions = resolve_instructions(&args);

        // Plan-first authoring: if there is an active cleanse plan, use task invariants/notes as the
        // default authoring instructions (and merge with any explicit user override instructions).
        let plan_opt = plan::load_cleanse_plan(ctx).await.ok().flatten();

        // Ensure minimal dbt project exists before writing artifacts. Any content the sanitizer
        // removed flows back via the returned Vec; persist it so the next author turn can
        // surface a stripped-content notice.
        match crate::transient_retry::retry_transient_default(
            "staging_ensure_minimal_project",
            || async { dbt.ensure_minimal_project(ctx.scope()).await },
        )
        .await
        {
            Ok(stripped) => {
                crate::plan_storage::persist_stripped_artifacts(ctx, stripped).await;
            }
            Err(e) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "batch_failure_kind": "unknown",
                    "datasets": dataset_ids.len(),
                    "written_keys": [],
                    "schema_key": Value::Null,
                    "notes": [],
                    "errors": [format!("failed to ensure minimal dbt project: {e}")],
                }));
            }
        }

        // Grounded gating (fail-fast, no side effects):
        // - Cleanse/silver must only operate on raw datasets (configured source_schema).
        // - The only fact we trust for dataset existence is query.schema(<fqn>) success.
        let cfg = crate::resolved_config_from_ctx(ctx).ok_or_else(|| {
            "resolved_config missing (cannot enforce raw dataset constraints)".to_string()
        })?;
        let providers = crate::de_config::de_config_from_resolved(cfg)
            .ok_or_else(|| "suite_config missing or invalid".to_string())?;
        let want_catalog = providers.warehouse.container.clone();
        let want_schema = providers.warehouse.namespace.clone();
        let mut schema_cols_by_ds: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();
        let gating_wh = crate::ctx_ext::actx_warehouse(ctx)
            .ok_or_else(|| "warehouse provider missing".to_string())?;
        let mut gating_errors: Vec<String> = Vec::new();
        for ds_ref in dataset_refs.iter() {
            let ds = ds_ref.fqn();
            let cat = ds_ref.catalog.clone();
            let db = ds_ref.schema.clone();
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
            // Prefer plan-recorded source_schema (single source of truth captured
            // at plan compilation time). Fall back to live warehouse only when the
            // plan doesn't have schema for this dataset.
            let plan_schema_cols: Option<Vec<(String, String)>> = plan_opt
                .as_ref()
                .and_then(|p| p.tasks.iter().find(|t| t.dataset_id == ds))
                .and_then(|t| {
                    if t.source_schema.is_empty() {
                        None
                    } else {
                        Some(
                            t.source_schema
                                .iter()
                                .map(|c| (c.name.clone(), c.data_type.clone()))
                                .collect(),
                        )
                    }
                });
            if let Some(cols) = plan_schema_cols {
                schema_cols_by_ds.insert(ds, cols);
            } else {
                let gating_ds = ds.clone();
                match crate::transient_retry::retry_transient_default(
                    "staging_model_schema_lookup",
                    || async { gating_wh.schema(&gating_ds).await },
                )
                .await
                {
                    Ok(cols) => {
                        tracing::warn!(
                            "staging_model: schema for {} came from live warehouse (plan source_schema was empty)",
                            gating_ds
                        );
                        schema_cols_by_ds.insert(ds, cols);
                    }
                    Err(e) => {
                        gating_errors.push(format!(
                            "{}: schema lookup failed (treating as fact): {}",
                            ds, e
                        ));
                    }
                }
            }
        }
        if !gating_errors.is_empty() {
            // IMPORTANT: do not write schema.yml or any models if the dataset facts are not proven.
            return Ok(serde_json::json!({
                "ok": false,
                "batch_failure_kind": "schema",
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
            .keyspace()
            .scoped_prefix(ctx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let schema_rel = project_fs::MODELS_SCHEMA_YML.to_string();
        let schema_key = format!("{}/{}", base, schema_rel);
        let existing_schema: Option<String> =
            match retry_get_bytes(ctx.storage().as_ref(), &schema_key).await {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
                Err(_) => None,
            };
        if existing_schema.is_none() {
            let seed = "version: 2\n".to_string();
            if let Err(e) = retry_put_bytes(
                ctx.storage().as_ref(),
                &schema_key,
                seed.as_bytes(),
                "text/yaml",
            )
            .await
            {
                return Ok(serde_json::json!({
                    "ok": false,
                    "batch_failure_kind": "schema",
                    "datasets": dataset_ids.len(),
                    "written_keys": [],
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": [format!("failed to write models/schema.yml: {e}")],
                }));
            }
        } else if let Some(existing) = existing_schema.as_deref() {
            // Deterministic `sources:` ownership: canonicalize and idempotently overwrite when
            // needed. Do not use unified-diff patch application (fragile vs LLM-edited YAML).
            let canonical =
                match project_fs::canonicalize_schema_yml(ctx, self.datasets.as_ref(), existing)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "batch_failure_kind": "schema",
                            "datasets": dataset_ids.len(),
                            "written_keys": [],
                            "schema_key": schema_key,
                            "notes": [],
                            "errors": [format!("failed to canonicalize models/schema.yml: {e}")],
                        }));
                    }
                };
            if canonical != existing {
                if let Err(e) = retry_put_bytes(
                    ctx.storage().as_ref(),
                    &schema_key,
                    canonical.as_bytes(),
                    "text/yaml",
                )
                .await
                {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "batch_failure_kind": "schema",
                        "datasets": dataset_ids.len(),
                        "written_keys": [],
                        "schema_key": schema_key,
                        "notes": [],
                        "errors": [format!("failed to write models/schema.yml: {e}")],
                    }));
                }
            }
        }

        let dialect = crate::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());
        let provider_name = crate::resolved_config_from_ctx(ctx)
            .and_then(|cfg| crate::de_config::de_config_from_resolved(cfg))
            .map(|p| p.warehouse.kind.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let stg_wh = crate::ctx_ext::actx_warehouse(ctx);
        let provider_prompt_rules =
            build_provider_prompt_rules(stg_wh.as_ref().map(|w| w.as_ref()));

        // Discover existing staging model files so we can update by semantic identity (source()),
        // not by filename (prevents duplicate staging models for the same dataset).
        let staging_prefix = format!("{}/models/staging/", base);
        let mut staging_files: Vec<(String, String)> = Vec::new(); // (rel_path, content)
        let mut unreadable_staging_rel_paths: Vec<String> = Vec::new();
        if let Ok(keys) = retry_list_prefix(ctx.storage().as_ref(), &staging_prefix).await {
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
                match retry_get_bytes(ctx.storage().as_ref(), &k).await {
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
            let sql_out = sql_first::strip_trailing_semicolon(&sql_out);
            if dataset_ids.len() != 1 {
                return Err("staging_model direct-write requires exactly one dataset (use a single-item args.dataset_ids).".to_string());
            }
            let ds_ref = dataset_refs[0].clone();
            let ds = ds_ref.fqn();
            let expected_db = ds_ref.schema;
            let expected_table = ds_ref.table;
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
                    "batch_failure_kind": "schema",
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
                    "batch_failure_kind": "schema",
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
                    "batch_failure_kind": "sql_runtime",
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
                    "batch_failure_kind": "sql_runtime",
                    "datasets": dataset_ids.len(),
                    "written_keys": written,
                    "schema_key": schema_key,
                    "notes": [],
                    "errors": errors,
                }));
            }
            let existing_opt = retry_get_bytes(ctx.storage().as_ref(), &key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string());
            let old_text = existing_opt.unwrap_or_default();
            let patch_text = project_fs::create_git_patch_text(
                &old_text,
                &sql_out,
                &rel_path,
                !old_text.is_empty(),
            )
            .map_err(|e| format!("failed to build staging SQL patch: {e}"))?;
            let outcome = project_fs::apply_patch(
                ctx,
                None,
                &rel_path,
                &patch_text,
                None,
                None,
                project_fs::PatchApplyKind::UnifiedDiff,
            )
            .await?;
            if let Err(e) = retry_put_bytes(
                ctx.storage().as_ref(),
                &key,
                outcome.content.as_bytes(),
                "text/sql",
            )
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

        let mut plan_output_field_names: Vec<String> = Vec::new();

        for ds_ref in dataset_refs.iter() {
            let ds = ds_ref.fqn();
            let expected_db = ds_ref.schema.clone();
            let expected_table = ds_ref.table.clone();
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
            let cols = schema_cols_by_ds.get(&ds).cloned().unwrap_or_default();
            let cols_json: Vec<Value> = cols
                .iter()
                .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                .collect();

            let sys = build_staging_sys_prompt(
                &provider_name,
                &dialect,
                &expected_db,
                &expected_table,
                &provider_prompt_rules,
            );

            let existing_sql = retry_get_bytes(ctx.storage().as_ref(), &key)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();

            let (
                plan_invariants,
                plan_checklist,
                plan_expected_model_path,
                plan_implementation_spec,
                plan_spec_digest,
            ) = plan_opt
                .as_ref()
                .and_then(|p| {
                    p.tasks.iter().find(|t| t.dataset_id == *ds).map(|t| {
                        (
                            t.invariants.clone(),
                            t.checklist.clone(),
                            t.expected_model_path.clone().unwrap_or_default(),
                            t.implementation_spec.clone(),
                            crate::authoring_contract::cleanse_task_spec_digest(&p.plan_key, t),
                        )
                    })
                })
                .unwrap_or_else(|| (vec![], vec![], String::new(), None, None));
            if plan_output_field_names.is_empty() {
                if let Some(spec) = plan_implementation_spec.as_ref() {
                    plan_output_field_names =
                        spec.output_fields.iter().map(|f| f.name.clone()).collect();
                }
            }
            let plan_instr = render_plan_driven_instructions(&plan_invariants, &plan_checklist);
            let effective_instructions = combine_instructions(&user_instructions, &plan_instr);

            // Existing SQL: if it already satisfies the active cleanse contract (digest + final
            // SELECT + expected source()), skip the LLM loop — same bar as post-author verification.
            if let Some(ref p) = plan_opt {
                if let Some(task) = p.tasks.iter().find(|t| t.dataset_id == *ds) {
                    if !existing_sql.trim().is_empty() {
                        let require_digest = plan_spec_digest.is_some();
                        let check = crate::authoring_contract::verify_cleanse_sql_contract(
                            p.plan_key.as_str(),
                            task,
                            &existing_sql,
                            require_digest,
                            Some((expected_db.as_str(), expected_table.as_str())),
                        );
                        if check.is_ok() {
                            succeeded_dataset_ids.push(ds.clone());
                            notes.push(format!(
                                "{ds}: existing staging SQL matches the active cleanse contract; skipped re-authoring."
                            ));
                            continue;
                        }
                    }
                }
            }

            let cols_for_sql: Vec<String> = cols
                .iter()
                .map(|(n, _t)| {
                    stg_wh
                        .as_ref()
                        .map(|w| w.quote_ident(n))
                        .unwrap_or_else(|| format!("\"{}\"", n))
                })
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
            let dsid = match stg_wh
                .as_ref()
                .ok_or_else(|| "warehouse missing".to_string())
                .and_then(|w| w.parse_dataset_fqn(&ds))
            {
                Ok(id) => id,
                Err(e) => {
                    errors.push(format!("{ds}: invalid dataset fqn: {e}"));
                    continue;
                }
            };
            if let Some(wh) = stg_wh.as_ref() {
                repl.insert("__SOURCE__".to_string(), wh.quote_fqn(&dsid));
            }

            let loop_config = engine::AuthorLoopConfig {
                max_tokens: max_tokens as usize,
                max_attempts,
                initial_prompt_id: "data_engineer.tools.staging_model.sql_first",
                repair_prompt_id: "data_engineer.tools.staging_model.sql_first_repair",
                reasoning_effort: crate::env_util::author_reasoning_effort(true),
                skip_warehouse_validation: false,
            };
            let cols_for_sql_clone = cols_for_sql.clone();
            let plan_output_fields_for_validation = plan_implementation_spec
                .as_ref()
                .map(|s| s.output_fields.clone())
                .unwrap_or_default();
            let loop_result = engine::sql_first_author_loop(
                ctx,
                &loop_config,
                || sys0.clone(),
                &user_value,
                &repl,
                &ds,
                &rel_path,
                move |d| {
                    if !d.sql.contains("__SOURCE__") {
                        return Err("draft SQL must reference __SOURCE__ placeholder".to_string());
                    }
                    if let Some(s) = sql_first::expand_select_star_from_placeholder(
                        &d.sql,
                        "__SOURCE__",
                        &cols_for_sql_clone,
                    ) {
                        d.sql = s;
                    }
                    crate::authoring_ir::compile_sql_first_draft(
                        &d.sql,
                        &d.notes,
                        &plan_output_fields_for_validation,
                    )?;
                    Ok(())
                },
            )
            .await;
            let outcome = match loop_result {
                Ok(o) => o,
                Err(errs) => {
                    errors.extend(errs);
                    continue;
                }
            };
            let expected_db2 = expected_db.clone();
            let expected_table2 = expected_table.clone();
            let mut materialize_repl = std::collections::HashMap::new();
            materialize_repl.insert(
                "__SOURCE__".to_string(),
                format!(
                    "{{{{ source(\"{}\", \"{}\") }}}}",
                    expected_db, expected_table
                ),
            );
            let write_result = engine::compile_and_write_model(
                ctx,
                &outcome.draft,
                plan_implementation_spec
                    .as_ref()
                    .map(|s| s.output_fields.as_slice())
                    .unwrap_or(&[]),
                &materialize_repl,
                |dbt_sql| {
                    if !contains_expected_source_call(dbt_sql, &expected_db2, &expected_table2) {
                        return Err(format!(
                            "materialized sql missing expected source(\"{expected_db2}\",\"{expected_table2}\")"
                        ));
                    }
                    Ok(())
                },
                &existing_sql,
                &rel_path,
                plan_spec_digest.as_deref(),
            )
            .await;
            match write_result {
                Ok(result) => {
                    written.push(result.key);
                    succeeded_dataset_ids.push(ds.clone());
                    for n in result.notes {
                        if !n.trim().is_empty() {
                            notes.push(format!("{}: {}", ds, n));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{ds}: {e}"));
                    continue;
                }
            }
        }

        info!(
            target: "staging_model",
            datasets = dataset_ids.len(),
            written = written.len(),
            "staging_model finished"
        );

        let out_notes = dedup_notes(notes, 50);
        let classified_kind = errors.iter().fold(FailureKind::Unknown, |acc, e| {
            let rhs = crate::tools::batch_sql_runner::classify_authoring_batch_failure_kind(e);
            if acc.is_transient() || rhs.is_transient() {
                crate::failure_kind::FailureKind::InfraTransient
            } else {
                acc
            }
        });

        let mut result = serde_json::json!({
            "ok": errors.is_empty(),
            "batch_failure_kind": if errors.is_empty() {
                Value::Null
            } else {
                serde_json::to_value(classified_kind)
                    .unwrap_or_else(|_| Value::String("unknown".to_string()))
            },
            "datasets": dataset_ids.len(),
            "written_keys": written,
            "schema_key": schema_key,
            "notes": out_notes,
            "errors": errors,
            "deferred_dataset_ids": deferred_dataset_ids,
            "succeeded_dataset_ids": succeeded_dataset_ids,
        });
        if !errors.is_empty() && !plan_output_field_names.is_empty() {
            result["expected_output_fields"] = serde_json::json!(plan_output_field_names);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::extract_source_calls;
    use crate::providers::{DbtProvider, QueryProvider, QueryResult};
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::{Arc, Mutex};

    #[test]
    fn resolve_dataset_ids_accepts_dataset_ids_array() {
        let args = serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders","AwsDataCatalog.test_raw.raw_orders"]});
        let got = resolve_dataset_ids(&args).expect("ok");
        assert_eq!(
            got.into_iter().map(|d| d.fqn()).collect::<Vec<_>>(),
            vec!["AwsDataCatalog.test_raw.raw_orders".to_string()]
        );
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
    fn staging_sys_prompt_uses_struct_dereference_for_dotted_columns() {
        let sys = build_staging_sys_prompt(
            "athena",
            "Amazon Athena (engine v3 / Trino SQL)",
            "picnic",
            "track_app_opened",
            "           - If Provider is athena (Trino SQL), DO NOT use initcap() (it is not registered). Avoid title-casing strings.\n",
        );
        assert!(sys.contains("schema_columns as ground truth"));
        assert!(sys.contains("struct dereference syntax"));
        assert!(sys.contains("Do NOT quote the entire dot-path as a single identifier"));
        assert!(sys.contains("output_fields[].name EXACTLY"));
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
            async fn ensure_minimal_project(
                &self,
                _scope: &RequestScope,
            ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String> {
                Ok(vec![])
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &crate::providers::DbtValidateArgs,
            ) -> Result<crate::providers::DbtValidateResult, String> {
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
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let cfg = Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: scope.clone(),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        });

        let warehouse: Arc<dyn crate::providers::WarehouseProvider> =
            Arc::new(crate::providers::warehouse::NullWarehouseProvider::default());
        let query_prov: Arc<dyn crate::providers::QueryProvider> = Arc::new(MockQuery);
        let dbt_prov: Arc<dyn crate::providers::DbtProvider> = Arc::new(MockDbt);
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(react_core::llm::NullModel::new()),
            storage.clone(),
            scope,
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("t1".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(cfg))
        .build();
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        ctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(query_prov)));
        ctx.set_capability(Arc::new(crate::ctx_ext::DbtCap(dbt_prov)));

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
            ctx.keyspace().scoped_prefix(ctx.scope(), &["dbt"]),
            crate::project_fs::MODELS_SCHEMA_YML
        );
        assert!(storage.get_bytes(&schema_key).await.is_err());
    }

    #[tokio::test]
    async fn staging_model_uses_cleanse_plan_invariants_and_notes_as_default_instructions() {
        #[derive(Clone)]
        struct MockDbt;
        #[async_trait]
        impl DbtProvider for MockDbt {
            async fn ensure_minimal_project(
                &self,
                _scope: &RequestScope,
            ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String> {
                Ok(vec![])
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &crate::providers::DbtValidateArgs,
            ) -> Result<crate::providers::DbtValidateResult, String> {
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
        impl crate::providers::DatasetCatalogProvider for MockWarehouse {
            async fn list_datasets(&self) -> Result<Vec<crate::providers::DatasetId>, String> {
                Ok(vec![crate::providers::DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_orders".to_string(),
                }])
            }

            async fn get_dataset_schema(
                &self,
                dataset: &crate::providers::DatasetId,
            ) -> Result<Vec<(String, String)>, String> {
                self.schema(&dataset.fqn()).await
            }

            async fn get_dataset_stats(
                &self,
                _dataset: &crate::providers::DatasetId,
                _max_fields: usize,
            ) -> Result<
                (
                    crate::providers::DatasetFieldStats,
                    crate::providers::DatasetStats,
                ),
                String,
            > {
                Err("not used".to_string())
            }

            fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
                crate::providers::ProviderEvidenceCapabilities::schema_only(
                    "mock warehouse provider",
                )
            }
        }
        impl crate::providers::WarehouseNaming for MockWarehouse {
            fn kind(&self) -> crate::de_config::WarehouseKind {
                crate::de_config::WarehouseKind::default()
            }
            fn parse_dataset_fqn(&self, fqn: &str) -> Result<crate::providers::DatasetId, String> {
                let parts: Vec<&str> = fqn.split('.').collect();
                if parts.len() != 3 {
                    return Err("expected <catalog>.<schema>.<table>".to_string());
                }
                Ok(crate::providers::DatasetId {
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
                    .find(|m| m.role == react_core::llm::ChatRole::User)
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
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let cfg = Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: scope.clone(),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        });

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let llm = Arc::new(CapturingLlm {
            resp: serde_json::json!({
                "sql": "select order_id as order_id_raw from __SOURCE__",
                "notes": []
            })
            .to_string(),
            captured_user_instructions: captured.clone(),
        });

        let warehouse: Arc<dyn crate::providers::WarehouseProvider> = Arc::new(MockWarehouse);
        let dbt_prov: Arc<dyn crate::providers::DbtProvider> = Arc::new(MockDbt);
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("t1".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(cfg))
        .build();
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        ctx.set_capability(Arc::new(crate::ctx_ext::DbtCap(dbt_prov)));

        // Seed an approved cleanse plan with invariants/checklist for this dataset.
        let ds = "AwsDataCatalog.test_raw.raw_orders".to_string();
        let plan_key = crate::plan::new_cleanse_plan_key(&ctx);
        let plan = crate::plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: crate::plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::CleanseTask {
                dataset_id: ds.clone(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec!["Staging grain: exactly 1 row per order_pk.".to_string()],
                implementation_spec: Some(crate::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "order_id_raw".to_string(),
                        kind: crate::plan::FieldKind::Raw,
                        lineage: vec![crate::plan::FieldLineage::column(
                            crate::plan::SourceFieldRef {
                                relation: None,
                                name: "order_id".to_string(),
                            },
                            crate::plan::lineage_role::PASSTHROUGH,
                        )],
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
                }),
                source_schema: vec![],
                status: crate::plan::TaskStatus::Pending,
                checklist: vec![
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "sql_model".to_string(),
                        label: "Author staging SQL".to_string(),
                        details: Some(
                            "Add a canonical order_pk and document behavior.".to_string(),
                        ),
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "schema_contract".to_string(),
                        label: "Author schema contract".to_string(),
                        details: None,
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "validate".to_string(),
                        label: "Validate model".to_string(),
                        details: None,
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec![ds.clone()]],
            work_groups: vec![
                crate::plan::PlanWorkGroup {
                    group_id: "wg_sql".to_string(),
                    label: "Author SQL".to_string(),
                    kind: crate::plan::WorkGroupKind::AuthorSql,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: ds.clone(),
                        checklist_item_id: "sql_model".to_string(),
                    }],
                    depends_on_group_ids: None,
                },
                crate::plan::PlanWorkGroup {
                    group_id: "wg_schema".to_string(),
                    label: "Author schema".to_string(),
                    kind: crate::plan::WorkGroupKind::AuthorSchema,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: ds.clone(),
                        checklist_item_id: "schema_contract".to_string(),
                    }],
                    depends_on_group_ids: Some(vec!["wg_sql".to_string()]),
                },
                crate::plan::PlanWorkGroup {
                    group_id: "wg_validate".to_string(),
                    label: "Validate".to_string(),
                    kind: crate::plan::WorkGroupKind::Validate,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: ds.clone(),
                        checklist_item_id: "validate".to_string(),
                    }],
                    depends_on_group_ids: Some(vec!["wg_schema".to_string()]),
                },
            ],
            mutations: vec![],
            progress: crate::plan::PlanProgress::default(),
        };
        crate::plan::save_cleanse_plan(&ctx, &plan).await.unwrap();

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

    #[derive(Clone)]
    struct PanicIfInvokedLlm;
    impl LargeLanguageModel for PanicIfInvokedLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            panic!("LLM invoked but existing staging SQL should satisfy cleanse contract");
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn staging_model_skips_llm_when_existing_cleanse_sql_matches_contract() {
        #[derive(Clone)]
        struct MockDbt;
        #[async_trait]
        impl DbtProvider for MockDbt {
            async fn ensure_minimal_project(
                &self,
                _scope: &RequestScope,
            ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String> {
                Ok(vec![])
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &crate::providers::DbtValidateArgs,
            ) -> Result<crate::providers::DbtValidateResult, String> {
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
                    Ok(vec![("order_id".to_string(), "string".to_string())])
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
        impl crate::providers::DatasetCatalogProvider for MockWarehouse {
            async fn list_datasets(&self) -> Result<Vec<crate::providers::DatasetId>, String> {
                Ok(vec![crate::providers::DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_orders".to_string(),
                }])
            }

            async fn get_dataset_schema(
                &self,
                dataset: &crate::providers::DatasetId,
            ) -> Result<Vec<(String, String)>, String> {
                self.schema(&dataset.fqn()).await
            }

            async fn get_dataset_stats(
                &self,
                _dataset: &crate::providers::DatasetId,
                _max_fields: usize,
            ) -> Result<
                (
                    crate::providers::DatasetFieldStats,
                    crate::providers::DatasetStats,
                ),
                String,
            > {
                Err("not used".to_string())
            }

            fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
                crate::providers::ProviderEvidenceCapabilities::schema_only(
                    "mock warehouse provider",
                )
            }
        }
        impl crate::providers::WarehouseNaming for MockWarehouse {
            fn kind(&self) -> crate::de_config::WarehouseKind {
                crate::de_config::WarehouseKind::default()
            }
            fn parse_dataset_fqn(&self, fqn: &str) -> Result<crate::providers::DatasetId, String> {
                let parts: Vec<&str> = fqn.split('.').collect();
                if parts.len() != 3 {
                    return Err("expected <catalog>.<schema>.<table>".to_string());
                }
                Ok(crate::providers::DatasetId {
                    catalog: parts[0].to_string(),
                    database: parts[1].to_string(),
                    table: parts[2].to_string(),
                })
            }
            fn quote_ident(&self, ident: &str) -> String {
                format!("\"{}\"", ident.replace('"', "\"\""))
            }
        }

        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let cfg = Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: scope.clone(),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        });

        let warehouse: Arc<dyn crate::providers::WarehouseProvider> = Arc::new(MockWarehouse);
        let dbt_prov: Arc<dyn crate::providers::DbtProvider> = Arc::new(MockDbt);
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(PanicIfInvokedLlm),
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("t1".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(cfg))
        .build();
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        ctx.set_capability(Arc::new(crate::ctx_ext::DbtCap(dbt_prov)));

        let ds = "AwsDataCatalog.test_raw.raw_orders".to_string();
        let plan_key = crate::plan::new_cleanse_plan_key(&ctx);
        let plan = crate::plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: crate::plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::CleanseTask {
                dataset_id: ds.clone(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(crate::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "order_id_raw".to_string(),
                        kind: crate::plan::FieldKind::Raw,
                        lineage: vec![crate::plan::FieldLineage::column(
                            crate::plan::SourceFieldRef {
                                relation: None,
                                name: "order_id".to_string(),
                            },
                            crate::plan::lineage_role::PASSTHROUGH,
                        )],
                        expression: "order_id".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                }),
                source_schema: vec![crate::plan::SourceColumnDef {
                    name: "order_id".to_string(),
                    data_type: "string".to_string(),
                }],
                status: crate::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec![ds.clone()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::plan::PlanProgress::default(),
        };
        let digest = crate::authoring_contract::cleanse_task_spec_digest(&plan_key, &plan.tasks[0])
            .expect("digest");
        let body = "{{ config(alias=\"stg_test_raw_raw_orders\") }}\n\nselect order_id as order_id_raw\nfrom {{ source('test_raw', 'raw_orders') }}";
        let sql_on_disk = crate::authoring_contract::add_sql_spec_digest(body, Some(&digest));
        assert!(crate::authoring_contract::verify_cleanse_sql_contract(
            &plan_key,
            &plan.tasks[0],
            &sql_on_disk,
            true,
            Some(("test_raw", "raw_orders")),
        )
        .is_ok());

        crate::plan::save_cleanse_plan(&ctx, &plan).await.unwrap();

        let base = ctx
            .keyspace()
            .scoped_prefix(ctx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let staging_key = format!("{}/models/staging/stg_test_raw_raw_orders.sql", base);
        storage
            .put_bytes(&staging_key, sql_on_disk.as_bytes(), "text/sql")
            .await
            .expect("seed staging sql");

        let tool = StagingModelTool {
            datasets: Some(Arc::new(MockWarehouse)),
        };
        let out = tool
            .call(serde_json::json!({"dataset_ids":[ds.clone()]}), &ctx)
            .await
            .expect("tool call");

        assert!(
            out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "{out:?}"
        );
        let succeeded: Vec<String> = serde_json::from_value(
            out.get("succeeded_dataset_ids")
                .cloned()
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(succeeded, vec![ds]);
        let written: Vec<String> =
            serde_json::from_value(out.get("written_keys").cloned().unwrap_or_default())
                .unwrap_or_default();
        assert!(
            written.is_empty(),
            "idempotent success should not rewrite SQL: {written:?}"
        );
    }

    #[tokio::test]
    async fn staging_model_direct_sql_canonicalizes_schema_yml_without_patch_apply() {
        #[derive(Clone)]
        struct MockDbt;
        #[async_trait]
        impl DbtProvider for MockDbt {
            async fn ensure_minimal_project(
                &self,
                _scope: &RequestScope,
            ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String> {
                Ok(vec![])
            }
            async fn write_model_sql(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _sql: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn write_metricflow_yaml(
                &self,
                _scope: &RequestScope,
                _rel_path: &str,
                _yaml_text: &str,
            ) -> Result<String, String> {
                Ok("k".to_string())
            }
            async fn validate_project(
                &self,
                _scope: &RequestScope,
                _args: &crate::providers::DbtValidateArgs,
            ) -> Result<crate::providers::DbtValidateResult, String> {
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
                    Ok(vec![("order_id".to_string(), "string".to_string())])
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
        impl crate::providers::DatasetCatalogProvider for MockWarehouse {
            async fn list_datasets(&self) -> Result<Vec<crate::providers::DatasetId>, String> {
                Ok(vec![crate::providers::DatasetId {
                    catalog: "AwsDataCatalog".to_string(),
                    database: "test_raw".to_string(),
                    table: "raw_orders".to_string(),
                }])
            }

            async fn get_dataset_schema(
                &self,
                dataset: &crate::providers::DatasetId,
            ) -> Result<Vec<(String, String)>, String> {
                self.schema(&dataset.fqn()).await
            }

            async fn get_dataset_stats(
                &self,
                _dataset: &crate::providers::DatasetId,
                _max_fields: usize,
            ) -> Result<
                (
                    crate::providers::DatasetFieldStats,
                    crate::providers::DatasetStats,
                ),
                String,
            > {
                Err("not used".to_string())
            }

            fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
                crate::providers::ProviderEvidenceCapabilities::schema_only(
                    "mock warehouse provider",
                )
            }
        }
        impl crate::providers::WarehouseNaming for MockWarehouse {
            fn kind(&self) -> crate::de_config::WarehouseKind {
                crate::de_config::WarehouseKind::default()
            }
            fn parse_dataset_fqn(&self, fqn: &str) -> Result<crate::providers::DatasetId, String> {
                let parts: Vec<&str> = fqn.split('.').collect();
                if parts.len() != 3 {
                    return Err("expected <catalog>.<schema>.<table>".to_string());
                }
                Ok(crate::providers::DatasetId {
                    catalog: parts[0].to_string(),
                    database: parts[1].to_string(),
                    table: parts[2].to_string(),
                })
            }
            fn quote_ident(&self, ident: &str) -> String {
                format!("\"{}\"", ident.replace('"', "\"\""))
            }
        }

        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let cfg = Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: scope.clone(),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        });

        let warehouse: Arc<dyn crate::providers::WarehouseProvider> = Arc::new(MockWarehouse);
        let dbt_prov: Arc<dyn crate::providers::DbtProvider> = Arc::new(MockDbt);
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(react_core::llm::NullModel::new()),
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("t1".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(cfg))
        .build();
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse.clone())));
        ctx.set_capability(Arc::new(crate::ctx_ext::DbtCap(dbt_prov)));

        let base = ctx
            .keyspace()
            .scoped_prefix(ctx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let schema_key = format!("{}/{}", base, crate::project_fs::MODELS_SCHEMA_YML);
        storage
            .put_bytes(&schema_key, b"version: 2\nsources: []\n", "text/yaml")
            .await
            .expect("seed schema");

        let ds = "AwsDataCatalog.test_raw.raw_orders";
        let sql = concat!(
            "{{ config(alias=\"stg_test_raw_raw_orders\") }}\n\n",
            "select order_id as order_id_raw\n",
            "from {{ source('test_raw', 'raw_orders') }}\n",
        );
        let tool = StagingModelTool {
            datasets: Some(warehouse),
        };
        let out = tool
            .call(serde_json::json!({"dataset_ids":[ds], "sql": sql}), &ctx)
            .await
            .expect("tool call");

        assert!(
            out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "{out:?}"
        );
        let schema_bytes = storage.get_bytes(&schema_key).await.expect("read schema");
        let yml = String::from_utf8_lossy(&schema_bytes);
        assert!(
            yml.contains("raw_orders"),
            "expected deterministic sources rebuild to register raw_orders; got:\n{yml}"
        );
    }
}
