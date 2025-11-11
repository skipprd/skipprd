use std::sync::Arc;
use arrow::array::{ArrayRef, StringArray};
use arrow::record_batch::RecordBatch as ArrowRecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::SessionContext;
use datafusion::arrow::datatypes::{DataType as ArrowDataType, Field as ArrowField};
use datafusion::arrow::datatypes::Schema as ArrowSchema2;
use tracing::debug;
use crate::sql::registry::PipelineRegistry;

pub async fn register_catalog(ctx: &SessionContext) {
    debug!("{} META: begin register_semantic_and_catalog (unified catalog, S3-only)", chrono::Utc::now().to_rfc3339());
    let cat_schema = Arc::new(ArrowSchema2::new(vec![
        ArrowField::new("namespace", ArrowDataType::Utf8, false),
        ArrowField::new("entity", ArrowDataType::Utf8, true),
        ArrowField::new("field", ArrowDataType::Utf8, false),
        ArrowField::new("description", ArrowDataType::Utf8, true),
        ArrowField::new("synonyms", ArrowDataType::Utf8, true),
        ArrowField::new("pii_sensitivity", ArrowDataType::Utf8, true),
        ArrowField::new("units_or_format", ArrowDataType::Utf8, true),
        ArrowField::new("role", ArrowDataType::Utf8, true),
        ArrowField::new("dimensions", ArrowDataType::Utf8, true),
        ArrowField::new("metrics", ArrowDataType::Utf8, true),
    ]));

    // Load pipeline registries from S3 under: <tenant>/<workspace>/*/manifest/registry.json
    let tenant = crate::helpers::configuration::Config::get_tenant();
    let workspace = crate::helpers::configuration::Config::get_workspace_name();
    let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
    let prefix = format!("{}/{}/", tenant, workspace);
    let s3 = crate::helpers::s3::get_s3_client().await;
    let mut token: Option<String> = None;
    let mut registries: Vec<PipelineRegistry> = Vec::new();
    loop {
        let mut req = s3.list_objects_v2().bucket(&bucket).prefix(&prefix);
        if let Some(t) = &token { req = req.continuation_token(t); }
        match req.send().await {
            Ok(resp) => {
                let contents = resp.contents();
                for obj in contents {
                    if let Some(key) = obj.key() {
                        if key.ends_with("/manifest/registry.json") {
                            if let Ok(val) = crate::helpers::s3::get_json(key).await {
                                if let Ok(pr) = serde_json::from_value::<PipelineRegistry>(val) {
                                    registries.push(pr);
                                }
                            }
                        }
                    }
                }
                if resp.is_truncated().unwrap_or(false) {
                    token = resp.next_continuation_token().map(|s| s.to_string());
                } else { break; }
            }
            Err(e) => {
                debug!("{} META: failed to list registries under s3://{}/{} err={:?}", chrono::Utc::now().to_rfc3339(), bucket, prefix, e);
                break;
            }
        }
    }
    debug!("{} META: registry pipelines loaded from S3: {}", chrono::Utc::now().to_rfc3339(), registries.len());
    for p in &registries { debug!("{} META: pipeline='{}' namespaces={}", chrono::Utc::now().to_rfc3339(), p.pipeline, p.namespaces.len()); }
    // Publish registries into cache for downstream consumers (ask/planner)
    {
        let mut cache = crate::sql::registry::REGISTRY_CACHE.write().await;
        cache.clear();
        cache.extend(registries.clone());
    }

    let mut cat_rows: Vec<(String, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = Vec::new();

    for pipe in &registries {
        for (ns, entry) in pipe.namespaces.iter() {
            debug!("{} META: ns='{}' catalog_key='{}'", chrono::Utc::now().to_rfc3339(), ns, entry.catalog_key);
            if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                let dims_str = if let Some(arr) = val.get("dimensions").and_then(|x| x.as_array()) { Some(arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<Vec<String>>().join(", ")) } else { None };
                let mets_str = if let Some(arr) = val.get("metrics").and_then(|x| x.as_array()) { Some(arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<Vec<String>>().join(", ")) } else { None };
                if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                    for f in fields {
                        let entity = f.get("entity").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if name.is_empty() { continue; }
                        let desc = f.get("description").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let syns = if let Some(arr) = f.get("synonyms").and_then(|x| x.as_array()) { Some(arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<Vec<String>>().join(", ")) } else { None };
                        let pii = f.get("pii_sensitivity").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let units = f.get("units_or_format").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let role = f.get("role").and_then(|x| x.as_str()).map(|s| s.to_string());
                        cat_rows.push((ns.clone(), entity.clone(), name, desc.clone(), syns.clone(), pii.clone(), units.clone(), role, dims_str.clone(), mets_str.clone()));
                    }
                }
            } else {
                debug!("{} META: failed to fetch catalog_key for ns='{}'", chrono::Utc::now().to_rfc3339(), ns);
            }
        }
    }

    debug!("{} META: catalog rows total: {}", chrono::Utc::now().to_rfc3339(), cat_rows.len());
    if !cat_rows.is_empty() {
        let ns_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>()));
        let entity_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.1.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let field_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>()));
        let desc_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.3.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let syn_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.4.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let pii_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.5.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let units_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.6.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let role_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.7.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let dims_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.8.clone().unwrap_or_default()).collect::<Vec<_>>()));
        let mets_arr: ArrayRef = Arc::new(StringArray::from(cat_rows.iter().map(|r| r.9.clone().unwrap_or_default()).collect::<Vec<_>>()));
        if let Ok(batch) = ArrowRecordBatch::try_new(cat_schema.clone(), vec![ns_arr, entity_arr, field_arr, desc_arr, syn_arr, pii_arr, units_arr, role_arr, dims_arr, mets_arr]) {
            let _ = ctx.register_table("catalog", Arc::new(MemTable::try_new(cat_schema.clone(), vec![vec![batch]]).unwrap()));
            debug!("{} META: catalog table registered", chrono::Utc::now().to_rfc3339());
        }
    }
    debug!("{} META: end register_semantic_and_catalog", chrono::Utc::now().to_rfc3339());
}


