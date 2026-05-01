use std::collections::HashSet;

use react_core::scope::RequestScope;

use crate::providers::{
    CatalogProvider, DataCatalog, DatasetProfile, EvidenceStatus, FieldProfile,
    KeyCandidateProfile, ProfileMetric, SemanticProfile, GLOBAL_SEMANTIC_DATASET_ID,
};

pub(crate) async fn build_and_store_semantic_profiles(
    catalog: &dyn CatalogProvider,
    scope: &RequestScope,
    dataset_ids: &HashSet<String>,
) -> Result<(), String> {
    let mut profiles = Vec::new();
    for dataset_id in dataset_ids {
        let Some(cat) = catalog.read_catalog(scope, dataset_id).await? else {
            continue;
        };
        let profile = build_dataset_profile(&cat);
        catalog
            .write_semantic_profile(
                scope,
                dataset_id,
                &SemanticProfile {
                    version: 1,
                    built_at_epoch_secs: now_epoch_secs(),
                    dataset_profiles: vec![profile.clone()],
                    notes: vec![],
                },
            )
            .await?;
        profiles.push(profile);
    }

    catalog
        .write_semantic_profile(
            scope,
            GLOBAL_SEMANTIC_DATASET_ID,
            &SemanticProfile {
                version: 1,
                built_at_epoch_secs: now_epoch_secs(),
                dataset_profiles: profiles,
                notes: vec!["aggregate-only semantic profile rollup".to_string()],
            },
        )
        .await?;
    Ok(())
}

pub(crate) fn build_dataset_profile(catalog: &DataCatalog) -> DatasetProfile {
    let row_count = catalog
        .dataset_stats
        .as_ref()
        .map(|stats| stats.approx_total_rows)
        .filter(|rows| *rows > 0);
    let mut fields = Vec::new();
    let mut key_candidates = Vec::new();

    for field in &catalog.fields {
        let stats = field.stats.as_ref();
        let total_count = stats.map(|s| s.total).or(row_count);
        let null_count = stats.map(|s| s.nulls).or_else(|| {
            catalog
                .dataset_stats
                .as_ref()
                .and_then(|ds| ds.nulls_by_field.get(&field.name).copied())
        });
        let approx_distinct_count = stats.and_then(|s| s.approx_distinct);
        fields.push(FieldProfile {
            field_name: field.name.clone(),
            status: if stats.is_some() {
                EvidenceStatus::Observed
            } else {
                EvidenceStatus::Unverified
            },
            total_count,
            null_count,
            approx_distinct_count,
            null_ratio: ratio_metric(null_count, total_count),
            distinct_ratio: ratio_metric(approx_distinct_count, total_count),
        });

        if let (Some(total), Some(nulls), Some(distinct)) =
            (total_count, null_count, approx_distinct_count)
        {
            if total > 0 && nulls == 0 && distinct == total {
                key_candidates.push(KeyCandidateProfile {
                    claim_id: format!("candidate_key:{}:{}", catalog.dataset_id, field.name),
                    field_names: vec![field.name.clone()],
                    status: EvidenceStatus::Observed,
                    total_count: Some(total),
                    null_count: Some(nulls),
                    approx_distinct_count: Some(distinct),
                });
            }
        }
    }

    DatasetProfile {
        dataset_id: catalog.dataset_id.clone(),
        row_count,
        fields,
        key_candidates,
        relationship_candidates: vec![],
    }
}

fn ratio_metric(numerator: Option<u64>, denominator: Option<u64>) -> Option<ProfileMetric> {
    let (Some(numerator), Some(denominator)) = (numerator, denominator) else {
        return None;
    };
    if denominator == 0 {
        return None;
    }
    Some(ProfileMetric {
        value: numerator as f64 / denominator as f64,
        exact: false,
    })
}

fn now_epoch_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use crate::providers::{CatalogField, DatasetStats, FieldStatsLite};

    use super::*;

    #[test]
    fn observes_candidate_key_only_from_aggregate_counts() {
        let catalog = DataCatalog {
            dataset_id: "AwsDataCatalog.db.orders".to_string(),
            catalog: "AwsDataCatalog".to_string(),
            database: "db".to_string(),
            table: "orders".to_string(),
            description: None,
            dimensions: vec![],
            metrics: vec![],
            fields: vec![CatalogField {
                entity: String::new(),
                name: "order_id".to_string(),
                data_type: None,
                root_column: None,
                field_path: None,
                structure_kind: None,
                access_descriptor: None,
                description: None,
                synonyms: None,
                pii_sensitivity: None,
                units_or_format: None,
                role: None,
                stats: Some(FieldStatsLite {
                    total: 10,
                    nulls: 0,
                    min_numeric: None,
                    max_numeric: None,
                    min_len: None,
                    max_len: None,
                    approx_distinct: Some(10),
                    histogram_bins: None,
                    histogram_min: None,
                    histogram_max: None,
                    last_updated_epoch_ms: 1,
                }),
            }],
            structure_index: Default::default(),
            dataset_stats: Some(DatasetStats {
                approx_total_rows: 10,
                earliest_ts: None,
                latest_ts: None,
                nulls_by_field: Default::default(),
            }),
            built_at_epoch_secs: None,
        };

        let profile = build_dataset_profile(&catalog);
        assert_eq!(profile.key_candidates.len(), 1);
        assert_eq!(profile.key_candidates[0].status, EvidenceStatus::Observed);
    }
}
