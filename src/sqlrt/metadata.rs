use crate::helpers::configuration::Config;
use datafusion::arrow::array::{ArrayRef, StringArray};
use datafusion::arrow::datatypes::Schema as ArrowSchema2;
use datafusion::arrow::datatypes::{DataType as ArrowDataType, Field as ArrowField};
use datafusion::arrow::record_batch::RecordBatch as ArrowRecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::SessionContext;
use std::sync::Arc;
use tracing::debug;

pub async fn register_catalog(config: &Config, ctx: &SessionContext) {
    debug!(
        "{} META: begin register_semantic_and_catalog (unified catalog, S3-only)",
        chrono::Utc::now().to_rfc3339()
    );
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

    // Load central registry
    let registry_opt = crate::sqlrt::registry::read_registry(config).await;
    let registry = match registry_opt {
        Some(r) => r,
        None => {
            debug!(
                "{} META: central registry missing; skipping catalog registration",
                chrono::Utc::now().to_rfc3339()
            );
            return;
        }
    };
    debug!(
        "{} META: registry pipelines loaded: {}",
        chrono::Utc::now().to_rfc3339(),
        registry.pipelines.len()
    );

    let mut cat_rows: Vec<(
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = Vec::new();

    for (_pipeline, nsmap) in registry.namespaces_by_pipeline.iter() {
        for (ns, entry) in nsmap.iter() {
            debug!(
                "{} META: ns='{}' catalog_key='{}'",
                chrono::Utc::now().to_rfc3339(),
                ns,
                entry.catalog_key
            );
            if let Ok(Some(val)) = crate::adapters::storage::get_storage(config)
                .get_json_opt(&entry.catalog_key)
                .await
            {
                let dims_str = if let Some(arr) = val.get("dimensions").and_then(|x| x.as_array()) {
                    Some(
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>()
                            .join(", "),
                    )
                } else {
                    None
                };
                let mets_str = if let Some(arr) = val.get("metrics").and_then(|x| x.as_array()) {
                    Some(
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>()
                            .join(", "),
                    )
                } else {
                    None
                };
                if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
                    for f in fields {
                        let entity = f
                            .get("entity")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        let name = f
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let desc = f
                            .get("description")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        let syns = if let Some(arr) = f.get("synonyms").and_then(|x| x.as_array()) {
                            Some(
                                arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .collect::<Vec<String>>()
                                    .join(", "),
                            )
                        } else {
                            None
                        };
                        let pii = f
                            .get("pii_sensitivity")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        let units = f
                            .get("units_or_format")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        let role = f
                            .get("role")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        cat_rows.push((
                            ns.clone(),
                            entity.clone(),
                            name,
                            desc.clone(),
                            syns.clone(),
                            pii.clone(),
                            units.clone(),
                            role,
                            dims_str.clone(),
                            mets_str.clone(),
                        ));
                    }
                }
            } else {
                debug!(
                    "{} META: failed to fetch catalog_key for ns='{}'",
                    chrono::Utc::now().to_rfc3339(),
                    ns
                );
            }
        }
    }

    debug!(
        "{} META: catalog rows total: {}",
        chrono::Utc::now().to_rfc3339(),
        cat_rows.len()
    );
    if !cat_rows.is_empty() {
        let ns_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
        ));
        let entity_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.1.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let field_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>(),
        ));
        let desc_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.3.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let syn_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.4.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let pii_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.5.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let units_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.6.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let role_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.7.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let dims_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.8.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        let mets_arr: ArrayRef = Arc::new(StringArray::from(
            cat_rows
                .iter()
                .map(|r| r.9.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        ));
        if let Ok(batch) = ArrowRecordBatch::try_new(
            cat_schema.clone(),
            vec![
                ns_arr, entity_arr, field_arr, desc_arr, syn_arr, pii_arr, units_arr, role_arr,
                dims_arr, mets_arr,
            ],
        ) {
            let _ = ctx.register_table(
                "catalog",
                Arc::new(MemTable::try_new(cat_schema.clone(), vec![vec![batch]]).unwrap()),
            );
            debug!(
                "{} META: catalog table registered",
                chrono::Utc::now().to_rfc3339()
            );
        }
    }
    debug!(
        "{} META: end register_semantic_and_catalog",
        chrono::Utc::now().to_rfc3339()
    );
}
