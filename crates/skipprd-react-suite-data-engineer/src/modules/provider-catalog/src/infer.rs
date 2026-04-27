use crate::stats_from_catalog::dataset_field_stats_from_catalog_json;
use crate::types::{SemanticField, SemanticFieldRole, SemanticModel};
use crate::utils::classify_field;
use react_core::keyspace::encode_key_component;
use react_core::keyspace::Keyspace;
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;
use react_suite_data_engineer::providers::DatasetFieldStats;
use std::sync::Arc;

pub async fn infer_semantic_model_async(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    scope: &RequestScope,
    dataset_id: &str,
) -> SemanticModel {
    let ns_stats: Option<DatasetFieldStats> = {
        let key = keyspace.scoped_key(
            scope,
            &[
                "catalog",
                &format!("{}.yaml", encode_key_component(dataset_id)),
            ],
        );
        match storage.get_json(&key).await {
            Ok(val) => dataset_field_stats_from_catalog_json(dataset_id, &val),
            Err(_) => None,
        }
    };
    let mut fields: Vec<SemanticField> = Vec::new();
    if let Some(ns) = ns_stats {
        for (fname, fstats) in ns.fields.iter() {
            let role = classify_field(fname, Some(fstats));
            fields.push(SemanticField {
                name: fname.clone(),
                role,
            });
        }
    }
    let mut model = SemanticModel {
        dataset_id: dataset_id.to_string(),
        catalog: String::new(),
        database: String::new(),
        table: String::new(),
        fields,
        dimensions: Vec::new(),
        metrics: Vec::new(),
    };
    for f in &model.fields {
        match f.role {
            SemanticFieldRole::Id
            | SemanticFieldRole::Timestamp
            | SemanticFieldRole::Categorical => model.dimensions.push(f.name.clone()),
            SemanticFieldRole::Metric => model.metrics.push(f.name.clone()),
            SemanticFieldRole::FreeText => {}
        }
    }
    model
}
