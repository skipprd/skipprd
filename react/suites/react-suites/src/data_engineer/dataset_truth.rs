use react_core::agent::AgentCtx;
use react_core::providers::WarehouseProvider;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use crate::data_engineer::references::DatasetRef;

pub type DatasetFqnParts = DatasetRef;

pub fn parse_dataset_fqn_3(s: &str) -> Option<DatasetFqnParts> {
    DatasetRef::parse(s)
}

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

impl GroundedDatasetSet {
    pub fn is_allowed(&self, dataset_id: &str) -> bool {
        self.allowed.contains(dataset_id)
    }
}

fn source_container_from_cfg(ctx: &AgentCtx) -> Option<String> {
    crate::config::resolved_config_from_ctx(ctx).map(|c| c.providers.warehouse.container.clone())
}

fn source_namespace_from_cfg(ctx: &AgentCtx) -> Option<String> {
    crate::config::resolved_config_from_ctx(ctx).map(|c| c.providers.warehouse.namespace.clone())
}

fn is_in_source_namespace(ctx: &AgentCtx, dataset_id: &str) -> bool {
    let Some(parts) = parse_dataset_fqn_3(dataset_id) else {
        return false;
    };
    // If namespace isn't configured (tests / minimal contexts), don't reject candidates on this axis.
    let Some(ns) = source_namespace_from_cfg(ctx) else {
        return true;
    };
    parts.schema == ns
}

fn is_in_source_container(ctx: &AgentCtx, dataset_id: &str) -> bool {
    let Some(parts) = parse_dataset_fqn_3(dataset_id) else {
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
    wh.schema(dataset_id).await.map(|_cols| ()).map_err(|e| e)
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
        if parse_dataset_fqn_3(&ds).is_none() {
            out.rejected.push(RejectedDataset {
                dataset_id: ds,
                reason: "invalid dataset_id format (expected <catalog>.<schema>.<table>)"
                    .to_string(),
            });
            continue;
        }
        if !is_in_source_container(ctx, &ds) {
            let parts = parse_dataset_fqn_3(&ds);
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
            let parts = parse_dataset_fqn_3(&ds);
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
    name.trim().to_ascii_lowercase().starts_with("stg_")
}

pub fn is_ref_only_gold_input(input: &str) -> bool {
    // Enforce “gold reads from silver”: only ref stg_* models.
    is_staging_model_name(input)
}

pub async fn discover_staging_models_from_storage(ctx: &AgentCtx) -> GroundedStagingModelSet {
    let mut out = GroundedStagingModelSet::default();
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string()
        + "/";

    // 1) models/staging/*.sql
    let staging_prefix = format!("{}models/staging/", base);
    let keys = ctx
        .storage
        .list_prefix(&staging_prefix)
        .await
        .unwrap_or_default();
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
    if let Ok(bytes) = ctx.storage.get_bytes(&manifest_key).await {
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

/// Helper to build candidate list for raw datasets from a list_datasets call.
pub fn candidates_from_list_datasets(listed: &[react_core::providers::DatasetId]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for ds in listed.iter() {
        out.insert(ds.fqn());
    }
    out.into_iter().collect()
}

/// Helper: map schema.yml `sources:` shape into candidate dataset ids.
pub fn candidates_from_schema_yml_sources(
    sources: &[(String, String)], // (schema, table) lowercased/trimmed
    catalog: &str,
) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for (schema, table) in sources.iter() {
        let s = schema.trim();
        let t = table.trim();
        if s.is_empty() || t.is_empty() {
            continue;
        }
        out.insert(format!("{}.{}.{}", catalog, s, t));
    }
    out.into_iter().collect()
}

/// Group dataset fqn strings by (catalog, schema) for schema.yml sources emission.
pub fn group_by_catalog_schema(
    dataset_ids: &BTreeSet<String>,
) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for id in dataset_ids.iter() {
        let Some(p) = parse_dataset_fqn_3(id) else {
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
