//! Metadata precedence for source truth grounding.
//!
//! The DE suite treats materialized warehouse/database relations as the source
//! of truth. Catalog entries are a run-scoped cache of those facts, refreshed at
//! bounded phase boundaries. Static SQL inference is only an authoring aid before
//! dbt materializes a relation and must never overwrite warehouse-reported
//! column names, types, or identifier casing.

use crate::providers::{DatasetId, WarehouseProvider};
use crate::references::DatasetRef;
use react_core::agent::AgentCtx;
use react_core::storage::{retry_get_bytes, retry_list_prefix};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectedDataset {
    pub dataset_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroundedDatasetSet {
    /// Canonical, proven dataset ids: `<catalog>.<schema>.<table>`
    pub allowed: BTreeSet<String>,
    /// All candidate dataset ids considered.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// Subset of candidates that were proven by schema().
    #[serde(default)]
    pub proven_by_schema: Vec<String>,
    /// Candidates rejected with explicit reasons.
    #[serde(default)]
    pub rejected: Vec<RejectedDataset>,
    /// Any warnings encountered while building the set (non-fatal).
    #[serde(default)]
    pub warnings: Vec<String>,
}

fn source_container_from_cfg(ctx: &AgentCtx) -> Option<String> {
    crate::ctx_ext::actx_providers_cfg(ctx).map(|p| p.warehouse.container.clone())
}

fn source_namespace_from_cfg(ctx: &AgentCtx) -> Option<String> {
    crate::ctx_ext::actx_providers_cfg(ctx).map(|p| p.warehouse.namespace.clone())
}

fn is_in_source_namespace(ctx: &AgentCtx, dataset_id: &str) -> bool {
    let Some(parts) = DatasetRef::parse(dataset_id) else {
        return false;
    };
    // If namespace isn't configured (tests / minimal contexts), don't reject candidates on this axis.
    let Some(ns) = source_namespace_from_cfg(ctx) else {
        return true;
    };
    parts.schema == ns
}

fn is_in_source_container(ctx: &AgentCtx, dataset_id: &str) -> bool {
    let Some(parts) = DatasetRef::parse(dataset_id) else {
        return false;
    };
    // If container isn't configured (tests / minimal contexts), don't reject candidates on this axis.
    let Some(cat) = source_container_from_cfg(ctx) else {
        return true;
    };
    parts.catalog == cat
}

async fn schema_proves_dataset(
    wh: &Arc<dyn WarehouseProvider>,
    dataset_id: &str,
) -> Result<(), String> {
    crate::transient_retry::retry_transient_default("schema_proves_dataset", || async {
        wh.schema(dataset_id).await.map(|_cols| ())
    })
    .await
}

/// Build a grounded dataset set for **raw/cleanse** (silver):
/// - only keeps datasets that are in configured raw schema AND proven by schema().
pub async fn build_grounded_raw_dataset_set(
    ctx: &AgentCtx,
    wh: &Arc<dyn WarehouseProvider>,
    candidates: &[String],
) -> GroundedDatasetSet {
    let mut out = GroundedDatasetSet::default();
    let mut uniq: BTreeSet<String> = BTreeSet::new();
    for c in candidates.iter() {
        let id = c.trim();
        if id.is_empty() {
            continue;
        }
        uniq.insert(id.to_string());
    }
    out.candidates = uniq.iter().cloned().collect();

    let cfg_container = source_container_from_cfg(ctx);
    let cfg_namespace = source_namespace_from_cfg(ctx);
    tracing::info!(
        "grounding: configured container={:?}, namespace={:?}, {} unique candidate(s)",
        cfg_container,
        cfg_namespace,
        uniq.len()
    );

    for ds in uniq.into_iter() {
        if DatasetRef::parse(&ds).is_none() {
            out.rejected.push(RejectedDataset {
                dataset_id: ds,
                reason: "invalid dataset_id format (expected <catalog>.<schema>.<table>)"
                    .to_string(),
            });
            continue;
        }
        if !is_in_source_container(ctx, &ds) {
            let parts = DatasetRef::parse(&ds);
            out.rejected.push(RejectedDataset {
                dataset_id: ds,
                reason: format!(
                    "dataset is not in configured source container (got {:?}, expected {:?})",
                    parts.as_ref().map(|p| &p.catalog),
                    cfg_container
                ),
            });
            continue;
        }
        if !is_in_source_namespace(ctx, &ds) {
            let parts = DatasetRef::parse(&ds);
            out.rejected.push(RejectedDataset {
                dataset_id: ds,
                reason: format!(
                    "not in configured raw source namespace (got {:?}, expected {:?})",
                    parts.as_ref().map(|p| &p.schema),
                    cfg_namespace
                ),
            });
            continue;
        }
        match schema_proves_dataset(wh, &ds).await {
            Ok(()) => {
                out.allowed.insert(ds.clone());
                out.proven_by_schema.push(ds);
            }
            Err(e) => {
                out.rejected.push(RejectedDataset {
                    dataset_id: ds,
                    reason: format!("schema lookup failed: {}", e),
                });
            }
        }
    }

    out
}

/// Build a set of available staging model names for **gold/model**:
/// discovered from dbt project files (staging SQL + manifest when present).
#[derive(Clone, Debug, Default)]
pub struct GroundedStagingModelSet {
    pub allowed_models: BTreeSet<String>,
    pub candidates: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn is_staging_model_name(name: &str) -> bool {
    normalize_staging_model_name(name).is_some()
}

pub fn normalize_staging_model_name(raw: &str) -> Option<String> {
    let mut t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with("{{") && t.ends_with("}}") && t.len() >= 4 {
        t = t[2..t.len() - 2].trim();
    }
    let lower = t.to_ascii_lowercase();
    let mut candidate = if lower.starts_with("ref(") && t.ends_with(')') {
        let inner = &t[4..t.len() - 1];
        inner
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '`')
            .to_string()
    } else {
        t.to_string()
    };
    if candidate.contains('/') {
        let stem = std::path::Path::new(candidate.as_str())
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !stem.trim().is_empty() {
            candidate = stem.trim().to_string();
        }
    }
    if candidate.contains('.') {
        if let Some(last) = candidate.rsplit('.').next() {
            candidate = last.trim().to_string();
        }
    }
    let normalized = candidate
        .trim()
        .trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_ascii_lowercase();
    if normalized.starts_with("stg_") {
        Some(normalized)
    } else {
        None
    }
}

/// `true` when this warehouse FQN should be treated as a **raw** cleanse source
/// (excludes existing staging/silver projections whose table basename is `stg_*`).
#[must_use]
pub fn is_cleanse_raw_source_dataset_candidate(dataset_id: &str) -> bool {
    let table = dataset_id.rsplit('.').next().unwrap_or("").trim();
    !is_staging_model_name(table)
}

/// Check whether `input` is a valid gold-model dependency: either a staging
/// model (`stg_*`) or another gold model in the same plan.
pub fn is_valid_gold_input(input: &str, plan_task_names: &BTreeSet<String>) -> bool {
    is_staging_model_name(input) || plan_task_names.contains(input.trim())
}

/// Append an IMMUTABLE FACTS block listing the exact model names that GOLD
/// models may reference via `ref()`.  Includes staging models (always) and
/// intra-plan gold models when a plan is available.
///
/// When `staging_schemas` is provided, each staging model is annotated with its
/// column list so the LLM can reason about structure.
pub fn enrich_query_with_staging_models(
    q: &str,
    staging: &GroundedStagingModelSet,
    staging_schemas: &crate::plan_types::SourceSchema,
) -> String {
    enrich_query_with_available_models(q, staging, staging_schemas, &[])
}

/// Like [`enrich_query_with_staging_models`] but also lists intra-plan gold
/// model names as valid `ref()` targets.
pub fn enrich_query_with_available_models(
    q: &str,
    staging: &GroundedStagingModelSet,
    staging_schemas: &crate::plan_types::SourceSchema,
    plan_gold_names: &[String],
) -> String {
    let mut out = q.to_string();
    if !staging.allowed_models.is_empty() || !plan_gold_names.is_empty() {
        out.push_str(
            "\n\nIMMUTABLE FACTS (available models \u{2014} GOLD models MUST reference these exact names via ref(); listed columns are warehouse/database-reported when materialized):\n",
        );
        out.push_str("Staging models:\n");
        for name in &staging.allowed_models {
            if let Some(cols) = staging_schemas.get(name.as_str()) {
                let cols_str: Vec<String> = cols
                    .iter()
                    .map(|c| format!("{} ({})", c.name, c.data_type))
                    .collect();
                out.push_str(&format!("- {} => [{}]\n", name, cols_str.join(", ")));
            } else {
                out.push_str(&format!("- {name}\n"));
            }
        }
        if !plan_gold_names.is_empty() {
            out.push_str("Intra-plan gold models (also valid ref() targets):\n");
            for name in plan_gold_names {
                out.push_str(&format!("- {name}\n"));
            }
        }
    }
    out
}

pub async fn discover_staging_models_from_storage(ctx: &AgentCtx) -> GroundedStagingModelSet {
    let mut out = GroundedStagingModelSet::default();
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string()
        + "/";

    // 1) models/staging/*.sql
    let staging_prefix = format!("{}models/staging/", base);
    let keys = match retry_list_prefix(ctx.storage().as_ref(), &staging_prefix).await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                "discover_staging_models_from_storage: list_prefix({}) failed: {e}",
                staging_prefix
            );
            out.warnings
                .push(format!("list_prefix failed for {staging_prefix}: {e}"));
            Vec::new()
        }
    };
    for k in keys {
        if !k.ends_with(".sql") || k.contains("/_versions/") {
            continue;
        }
        let rel = k.strip_prefix(&base).unwrap_or(&k).to_string();
        let name = std::path::Path::new(&rel)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_staging_model_name(&name) {
            out.allowed_models.insert(name.clone());
            out.candidates.push(name);
        }
    }

    // 2) target/manifest.json (best-effort enrichment)
    let manifest_key = format!("{}target/manifest.json", base);
    if let Ok(bytes) = retry_get_bytes(ctx.storage().as_ref(), &manifest_key).await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(nodes) = v.get("nodes").and_then(|n| n.as_object()) {
                for (_uid, node) in nodes.iter() {
                    let rt = node
                        .get("resource_type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    if rt != "model" {
                        continue;
                    }
                    let name = node
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() && is_staging_model_name(name) {
                        let fp = node
                            .get("original_file_path")
                            .or_else(|| node.get("path"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if fp.starts_with("models/staging/") {
                            out.allowed_models.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }

    out
}

fn source_column_def_from_catalog_field(
    f: &crate::providers::catalog_types::CatalogField,
) -> crate::plan_types::SourceColumnDef {
    let name = f
        .field_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| f.name.trim())
        .to_string();
    crate::plan_types::SourceColumnDef {
        name,
        data_type: f.data_type.clone().unwrap_or_else(|| "unknown".to_string()),
    }
}

/// Fetch column schemas for a set of raw/source dataset IDs.
/// Returns both the typed `SourceSchema` map (for compile-time threading) and
/// a rendered prompt block string (for LLM injection).
///
/// This is the single DRY helper reused by both cleanse and model paths.
///
/// Precedence:
/// 1. The catalog cache, which bootstrap refreshes from provider
///    `get_dataset_schema` / `get_dataset_stats` once per run.
/// 2. A bounded direct warehouse fallback only when the cache is missing.
/// 3. No static SQL inference here; downstream gates catch any remaining gaps.
pub async fn build_catalog_column_context(
    ctx: &AgentCtx,
    dataset_ids: &[String],
) -> (crate::plan_types::SourceSchema, String) {
    use crate::plan_types::{SourceColumnDef, SourceSchema};

    let mut schema_map: SourceSchema = BTreeMap::new();
    let catalog = crate::ctx_ext::actx_catalog(ctx);
    let query_provider = crate::ctx_ext::actx_query(ctx);

    if let Some(cat) = catalog {
        for ds_id in dataset_ids {
            let id = ds_id.trim();
            if id.is_empty() {
                continue;
            }
            match cat.read_catalog(ctx.scope(), id).await {
                Ok(Some(dc)) => {
                    let cols: Vec<SourceColumnDef> = dc
                        .fields
                        .iter()
                        .map(source_column_def_from_catalog_field)
                        .collect();
                    if !cols.is_empty() {
                        schema_map.insert(id.to_string(), cols);
                    }
                }
                Ok(None) => {
                    tracing::debug!("build_catalog_column_context: no catalog entry for {}", id);
                }
                Err(e) => {
                    tracing::warn!(
                        "build_catalog_column_context: read_catalog failed for {}: {}",
                        id,
                        e
                    );
                }
            }
        }
    } else {
        tracing::warn!("build_catalog_column_context: CatalogProvider not available");
    }

    if let Some(qp) = query_provider {
        for ds_id in dataset_ids {
            let id = ds_id.trim();
            if id.is_empty() || schema_map.contains_key(id) {
                continue;
            }
            match qp.schema(id).await {
                Ok(cols) if !cols.is_empty() => {
                    tracing::info!(
                        "build_catalog_column_context: warehouse fallback for {} ({} cols)",
                        id,
                        cols.len()
                    );
                    let src_cols: Vec<SourceColumnDef> = cols
                        .into_iter()
                        .map(|(name, data_type)| SourceColumnDef { name, data_type })
                        .collect();
                    schema_map.insert(id.to_string(), src_cols);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        "build_catalog_column_context: warehouse schema fallback failed for {}: {}",
                        id,
                        e
                    );
                }
            }
        }
    }

    let prompt_block = render_source_schema_prompt_block(&schema_map);
    (schema_map, prompt_block)
}

/// Render a `SourceSchema` map into a bounded prompt block for LLM injection.
pub fn render_source_schema_prompt_block(schema: &crate::plan_types::SourceSchema) -> String {
    if schema.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("\n\nAUTHORITATIVE SCHEMAS (warehouse/database-reported source columns \u{2014} column lineage uses lineage_kind=column with lineage[].source.name; use nested leaf paths when listed here):\n");
    for (ds_id, cols) in schema.iter() {
        let cols_str: Vec<String> = cols
            .iter()
            .map(|c| format!("{} ({})", c.name, c.data_type))
            .collect();
        out.push_str(&format!("  {}: [{}]\n", ds_id, cols_str.join(", ")));
    }
    out
}

/// Resolve the effective dbt target_schema, falling back to `scope.project_id`
/// (matching the dbt profile generation logic) when not explicitly configured.
fn effective_target_schema(
    ctx: &AgentCtx,
) -> Option<(String, crate::de_config::ProvidersResolved)> {
    let cfg = crate::resolved_config_from_ctx(ctx)?;
    let p = crate::de_config::de_config_from_resolved(cfg)?;
    let ts = p.dbt.naming.target_schema.trim().to_string();
    let base = if ts.is_empty() {
        crate::dbt::profile::derive_scope_db_name(cfg)
    } else {
        ts
    };
    if base.is_empty() {
        None
    } else {
        Some((base, p))
    }
}

fn dbt_relation_catalog_schema(ctx: &AgentCtx, suffix: &str) -> Option<(String, String)> {
    let (base_schema, p) = effective_target_schema(ctx)?;
    let container = p.warehouse.container.trim().to_string();
    let suffix = suffix.trim();
    if container.is_empty() || suffix.is_empty() {
        return None;
    }
    Some((container, format!("{}_{}", base_schema, suffix)))
}

/// Canonical warehouse prefix for staged silver relations, e.g.
/// `AwsDataCatalog.example_silver`.
pub fn staging_relation_prefix(ctx: &AgentCtx) -> Option<String> {
    let (_, p) = effective_target_schema(ctx)?;
    let (container, schema) = dbt_relation_catalog_schema(ctx, &p.dbt.naming.silver_suffix)?;
    Some(format!("{}.{}", container, schema))
}

/// Canonical warehouse prefix for gold (marts/core) relations, e.g.
/// `AwsDataCatalog.example_warehouse`.
pub fn gold_relation_prefix(ctx: &AgentCtx) -> Option<String> {
    let (_, p) = effective_target_schema(ctx)?;
    let (container, schema) = dbt_relation_catalog_schema(ctx, &p.dbt.naming.gold_suffix)?;
    Some(format!("{}.{}", container, schema))
}

/// Query the warehouse for output schemas of materialized staging models and
/// merge them into `source_schemas` keyed by `stg_*` name.
///
/// This deliberately overwrites any existing `stg_*` entries because those may
/// be inferred from authored SQL/schema.yml. Once dbt has materialized a
/// relation, warehouse-reported fields and casing are canonical.
pub async fn record_staging_output_schemas(
    ctx: &AgentCtx,
    staging_model_names: &BTreeSet<String>,
    source_schemas: &mut crate::plan_types::SourceSchema,
) {
    let wh = match crate::ctx_ext::actx_warehouse(ctx) {
        Some(wh) => wh,
        None => return,
    };
    let Some((catalog, schema)) = effective_target_schema(ctx)
        .and_then(|(_, p)| dbt_relation_catalog_schema(ctx, &p.dbt.naming.silver_suffix))
    else {
        return;
    };

    let pending: Vec<String> = staging_model_names
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    let concurrency = crate::providers::QueryProvider::max_concurrency(wh.as_ref())
        .clamp(1, 8)
        .min(pending.len().max(1));
    let mut names = pending.into_iter();
    let mut lookups = tokio::task::JoinSet::new();

    while lookups.len() < concurrency {
        let Some(name) = names.next() else {
            break;
        };
        spawn_staging_output_schema_lookup(
            &mut lookups,
            wh.clone(),
            catalog.clone(),
            schema.clone(),
            name,
        );
    }

    while let Some(joined) = lookups.join_next().await {
        let Some(name) = names.next() else {
            match joined {
                Ok((name, fqn, result)) => {
                    merge_staging_output_schema_result(source_schemas, name, fqn, result);
                }
                Err(e) => {
                    tracing::debug!("record_staging_output_schemas: lookup task failed: {}", e);
                }
            }
            continue;
        };
        match joined {
            Ok((finished_name, fqn, result)) => {
                merge_staging_output_schema_result(source_schemas, finished_name, fqn, result);
            }
            Err(e) => {
                tracing::debug!("record_staging_output_schemas: lookup task failed: {}", e);
            }
        }
        spawn_staging_output_schema_lookup(
            &mut lookups,
            wh.clone(),
            catalog.clone(),
            schema.clone(),
            name,
        );
    }
}

fn spawn_staging_output_schema_lookup(
    lookups: &mut tokio::task::JoinSet<(String, String, Result<Vec<(String, String)>, String>)>,
    wh: Arc<dyn WarehouseProvider>,
    catalog: String,
    schema: String,
    name: String,
) {
    lookups.spawn(async move {
        let relation_id = DatasetId {
            catalog,
            database: schema,
            table: name.clone(),
        };
        let lookup_id = wh.dbt_model_relation_lookup_id(&relation_id);
        let fqn = wh.format_dbt_model_relation_fqn(&relation_id);
        let result =
            crate::transient_retry::retry_transient_default("staging_output_schema", || async {
                wh.get_dataset_schema(&lookup_id).await
            })
            .await;
        (name, fqn, result)
    });
}

fn merge_staging_output_schema_result(
    source_schemas: &mut crate::plan_types::SourceSchema,
    name: String,
    fqn: String,
    result: Result<Vec<(String, String)>, String>,
) {
    match result {
        Ok(cols) if !cols.is_empty() => {
            let defs: Vec<crate::plan_types::SourceColumnDef> = cols
                .into_iter()
                .map(|(n, t)| crate::plan_types::SourceColumnDef {
                    name: n,
                    data_type: t,
                })
                .collect();
            source_schemas.insert(name.to_string(), defs);
        }
        Ok(_) => {
            tracing::debug!(
                "record_staging_output_schemas: empty schema for {} (not yet materialized?)",
                fqn
            );
        }
        Err(e) => {
            tracing::debug!(
                "record_staging_output_schemas: schema lookup failed for {}: {}",
                fqn,
                e
            );
        }
    }
}

/// All discovery results from a single pass, threaded through the plan pipeline
/// so no downstream function re-fetches. Built once at the top of `execute_plan_phase`.
pub struct PlanDiscoveryContext {
    /// Every dataset FQN returned by `list_datasets()`.
    pub dataset_fqns: Vec<String>,
    /// Source tables to cleanse: discovered datasets from the configured raw schema,
    /// excluding warehouse relations whose basename is an existing staging model (`stg_*`).
    pub raw_dataset_ids: BTreeSet<String>,
    /// Column schemas keyed by dataset FQN (raw) or staging model name.
    pub source_schemas: crate::plan_types::SourceSchema,
    /// Staging models discovered from storage (model track only).
    pub staging: Option<GroundedStagingModelSet>,
}

/// Group dataset fqn strings by (catalog, schema) for schema.yml sources emission.
pub fn group_by_catalog_schema(
    dataset_ids: &BTreeSet<String>,
) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for id in dataset_ids.iter() {
        let Some(p) = DatasetRef::parse(id) else {
            continue;
        };
        out.entry((p.catalog, p.schema)).or_default().push(p.table);
    }
    for (_k, v) in out.iter_mut() {
        v.sort();
        v.dedup();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::merge_staging_output_schema_result;
    use crate::plan_types::SourceColumnDef;
    use std::collections::BTreeMap;

    #[test]
    fn catalog_field_mapping_prefers_nested_field_path() {
        use crate::providers::catalog_types::{CatalogField, StatsStatus};
        let f = CatalogField {
            entity: "e".to_string(),
            name: "context".to_string(),
            data_type: Some("struct".to_string()),
            root_column: None,
            field_path: Some("context.session.id".to_string()),
            structure_kind: None,
            access_descriptor: None,
            description: None,
            synonyms: None,
            pii_sensitivity: None,
            units_or_format: None,
            role: None,
            stats_status: StatsStatus::default(),
            stats: None,
        };
        let col = super::source_column_def_from_catalog_field(&f);
        assert_eq!(col.name, "context.session.id");
    }

    #[test]
    fn cleanse_raw_source_dataset_candidate_rejects_stg_basename() {
        assert!(!super::is_cleanse_raw_source_dataset_candidate(
            "AwsDataCatalog.picnic.stg_orders"
        ));
        assert!(super::is_cleanse_raw_source_dataset_candidate(
            "AwsDataCatalog.raw.events"
        ));
    }

    #[test]
    fn staging_output_schema_result_overwrites_inferred_schema_with_warehouse_names() {
        let mut schemas = BTreeMap::new();
        schemas.insert(
            "stg_orders".to_string(),
            vec![SourceColumnDef {
                name: "total_amount".to_string(),
                data_type: "number".to_string(),
            }],
        );

        merge_staging_output_schema_result(
            &mut schemas,
            "stg_orders".to_string(),
            "ANALYTICS.SILVER.STG_ORDERS".to_string(),
            Ok(vec![("TOTAL_AMOUNT".to_string(), "NUMBER".to_string())]),
        );

        assert_eq!(schemas["stg_orders"][0].name, "TOTAL_AMOUNT");
    }
}
