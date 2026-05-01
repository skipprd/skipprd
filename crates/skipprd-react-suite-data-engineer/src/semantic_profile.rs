use std::collections::{BTreeSet, HashSet};

use react_core::agent::AgentCtx;
use react_core::scope::RequestScope;

use crate::providers::{
    CatalogProvider, DataCatalog, DatasetProfile, EvidenceStatus, FieldProfile,
    KeyCandidateProfile, ProfileMetric, SemanticClaimKind, SemanticClaimRef, SemanticProfile,
    GLOBAL_SEMANTIC_DATASET_ID,
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
        let distinct_count_exact = stats.map(|s| s.distinct_count_exact).unwrap_or_default();
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
            distinct_count_exact,
            null_ratio: ratio_metric(null_count, total_count),
            distinct_ratio: ratio_metric(approx_distinct_count, total_count),
        });

        if let (Some(total), Some(nulls), Some(distinct)) =
            (total_count, null_count, approx_distinct_count)
        {
            if distinct_count_exact && total > 0 && nulls == 0 && distinct == total {
                key_candidates.push(KeyCandidateProfile {
                    claim_id: format!("candidate_key:{}:{}", catalog.dataset_id, field.name),
                    field_names: vec![field.name.clone()],
                    status: EvidenceStatus::Observed,
                    total_count: Some(total),
                    null_count: Some(nulls),
                    approx_distinct_count: Some(distinct),
                    distinct_count_exact,
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

pub(crate) async fn attach_semantic_profile_claim_refs_to_model_plan(
    ctx: &AgentCtx,
    plan: &mut crate::plan_types::ModelPlan,
) -> Result<usize, String> {
    let Some(catalog) = crate::ctx_ext::actx_catalog(ctx) else {
        return Ok(0);
    };
    let Some(profile) = catalog
        .read_semantic_profile(ctx.scope(), GLOBAL_SEMANTIC_DATASET_ID)
        .await?
    else {
        return Ok(0);
    };

    let mut attached = 0;
    for task in plan.tasks.iter_mut() {
        let refs = semantic_claim_refs_for_model_task(&profile, task);
        let Some(spec) = task.implementation_spec.as_mut() else {
            continue;
        };
        attached += attach_claim_refs_to_spec(spec, refs);
    }
    attached += attach_intra_plan_claim_refs(plan);

    if attached > 0 {
        plan.project_snapshot.insert(
            "semantic_profile_evidence_attach",
            serde_json::json!({
                "attached_claim_refs": attached,
                "source": "semantic_profile",
                "dataset_profiles": profile.dataset_profiles.len(),
            }),
        );
    }

    Ok(attached)
}

fn attach_claim_refs_to_spec(
    spec: &mut crate::plan_types::ModelImplementationSpec,
    refs: impl IntoIterator<Item = SemanticClaimRef>,
) -> usize {
    let mut attached = 0;
    for claim_ref in refs {
        if !claim_ref.status.authoring_safe() {
            continue;
        }
        if spec.evidence_claim_refs.iter().any(|existing| {
            existing.claim_id == claim_ref.claim_id && existing.kind == claim_ref.kind
        }) {
            continue;
        }
        spec.evidence_claim_refs.push(claim_ref);
        attached += 1;
    }
    attached
}

pub(crate) fn attach_intra_plan_claim_refs(plan: &mut crate::plan_types::ModelPlan) -> usize {
    let mut total_attached = 0;
    loop {
        let refs_by_task = plan
            .tasks
            .iter()
            .filter_map(|task| {
                let refs = task
                    .implementation_spec
                    .as_ref()?
                    .evidence_claim_refs
                    .iter()
                    .filter(|claim| claim.status.authoring_safe())
                    .cloned()
                    .collect::<Vec<_>>();
                Some((normalize_component(&task.name), refs))
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        let mut attached_this_pass = 0;
        for task in plan.tasks.iter_mut() {
            let input_names = task
                .inputs
                .iter()
                .chain(task.grounded_inputs.iter().map(|input| &input.input_name))
                .map(|input| normalize_component(input))
                .collect::<BTreeSet<_>>();
            let refs = input_names
                .iter()
                .filter_map(|input| refs_by_task.get(input))
                .flat_map(|refs| refs.iter().cloned())
                .collect::<Vec<_>>();
            if let Some(spec) = task.implementation_spec.as_mut() {
                attached_this_pass += attach_claim_refs_to_spec(spec, refs);
            }
        }

        if attached_this_pass == 0 {
            break;
        }
        total_attached += attached_this_pass;
    }
    total_attached
}

pub(crate) fn semantic_claim_refs_for_model_task(
    profile: &SemanticProfile,
    task: &crate::plan_types::ModelTask,
) -> Vec<SemanticClaimRef> {
    let mut refs = Vec::new();
    let mut seen = BTreeSet::<(String, String)>::new();

    for dataset_profile in matched_input_profiles(profile, task) {
        for key in &dataset_profile.key_candidates {
            if !key.status.authoring_safe() {
                continue;
            }
            let claim_ref = SemanticClaimRef {
                claim_id: key.claim_id.clone(),
                kind: SemanticClaimKind::CandidateKey,
                status: key.status.clone(),
            };
            if seen.insert((claim_ref.claim_id.clone(), format!("{:?}", claim_ref.kind))) {
                refs.push(claim_ref);
            }
        }
        for rel in &dataset_profile.relationship_candidates {
            if !rel.status.authoring_safe() {
                continue;
            }
            let claim_ref = SemanticClaimRef {
                claim_id: rel.claim_id.clone(),
                kind: SemanticClaimKind::Relationship,
                status: rel.status.clone(),
            };
            if seen.insert((claim_ref.claim_id.clone(), format!("{:?}", claim_ref.kind))) {
                refs.push(claim_ref);
            }
        }
    }

    refs
}

fn matched_input_profiles<'a>(
    profile: &'a SemanticProfile,
    task: &crate::plan_types::ModelTask,
) -> Vec<&'a DatasetProfile> {
    let mut out = Vec::new();
    let mut matched_ids = BTreeSet::<String>::new();

    for input in &task.grounded_inputs {
        let relation_norm = normalize_relation(&input.relation_fqn);
        let mut matches: Vec<&DatasetProfile> = profile
            .dataset_profiles
            .iter()
            .filter(|p| normalize_relation(&p.dataset_id) == relation_norm)
            .collect();

        if matches.is_empty() {
            let unique_table_matches = unique_table_matches(profile, &input.input_name);
            if unique_table_matches.len() == 1 {
                matches = unique_table_matches;
            }
        }

        for dataset_profile in matches {
            if matched_ids.insert(dataset_profile.dataset_id.clone()) {
                out.push(dataset_profile);
            }
        }
    }

    if out.is_empty() {
        for input_name in &task.inputs {
            let unique_table_matches = unique_table_matches(profile, input_name);
            if unique_table_matches.len() == 1 {
                let dataset_profile = unique_table_matches[0];
                if matched_ids.insert(dataset_profile.dataset_id.clone()) {
                    out.push(dataset_profile);
                }
            }
        }
    }

    out
}

fn unique_table_matches<'a>(
    profile: &'a SemanticProfile,
    input_name: &str,
) -> Vec<&'a DatasetProfile> {
    let input_aliases = table_aliases(input_name);
    profile
        .dataset_profiles
        .iter()
        .filter(|p| !table_aliases(&p.dataset_id).is_disjoint(&input_aliases))
        .collect()
}

fn table_aliases(value: &str) -> BTreeSet<String> {
    let table = last_relation_component(value);
    let mut aliases = BTreeSet::new();
    if !table.is_empty() {
        aliases.insert(table.clone());
    }
    if let Some(raw_name) = table.strip_prefix("stg_") {
        if !raw_name.is_empty() {
            aliases.insert(raw_name.to_string());
        }
    }
    aliases
}

fn normalize_relation(value: &str) -> String {
    value
        .split('.')
        .map(normalize_component)
        .collect::<Vec<_>>()
        .join(".")
}

fn normalize_component(value: &str) -> String {
    value.trim().trim_matches('"').to_ascii_lowercase()
}

fn last_relation_component(value: &str) -> String {
    value
        .rsplit('.')
        .next()
        .map(normalize_component)
        .unwrap_or_default()
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
                    distinct_count_exact: true,
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

    #[test]
    fn does_not_observe_candidate_key_from_approximate_distinct_count() {
        let mut catalog = DataCatalog {
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
                    distinct_count_exact: false,
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
        assert!(profile.key_candidates.is_empty());

        catalog.fields[0]
            .stats
            .as_mut()
            .expect("stats")
            .distinct_count_exact = true;
        let profile = build_dataset_profile(&catalog);
        assert_eq!(profile.key_candidates.len(), 1);
    }

    #[test]
    fn returns_claim_refs_for_grounded_model_inputs() {
        let profile = SemanticProfile {
            version: 1,
            built_at_epoch_secs: None,
            dataset_profiles: vec![DatasetProfile {
                dataset_id: "ANALYTICS.DBT_SKIPPR.STG_ORDERS".to_string(),
                row_count: Some(10),
                fields: vec![],
                key_candidates: vec![KeyCandidateProfile {
                    claim_id: "candidate_key:ANALYTICS.DBT_SKIPPR.STG_ORDERS:ORDER_ID".to_string(),
                    field_names: vec!["ORDER_ID".to_string()],
                    status: EvidenceStatus::Observed,
                    total_count: Some(10),
                    null_count: Some(0),
                    approx_distinct_count: Some(10),
                    distinct_count_exact: true,
                }],
                relationship_candidates: vec![],
            }],
            notes: vec![],
        };
        let task = crate::plan_types::ModelTask {
            name: "fct_orders".to_string(),
            folder: Default::default(),
            goal: String::new(),
            inputs: vec!["stg_orders".to_string()],
            expected_model_path: None,
            invariants: vec![],
            implementation_spec: None,
            source_schema: vec![],
            grounded_inputs: vec![crate::plan_types::GroundedModelInput {
                input_name: "stg_orders".to_string(),
                model_rel_path: "models/staging/stg_orders.sql".to_string(),
                relation_fqn: "analytics.dbt_skippr.stg_orders".to_string(),
                source_schema: vec![],
            }],
            status: Default::default(),
            checklist: vec![],
        };

        let refs = semantic_claim_refs_for_model_task(&profile, &task);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, SemanticClaimKind::CandidateKey);
        assert_eq!(refs[0].status, EvidenceStatus::Observed);
    }

    #[test]
    fn returns_claim_refs_for_staging_inputs_backed_by_raw_profiles() {
        let profile = SemanticProfile {
            version: 1,
            built_at_epoch_secs: None,
            dataset_profiles: vec![DatasetProfile {
                dataset_id: "ANALYTICS.RAW.RAW_CARGO_BUILD_1_ORDERS".to_string(),
                row_count: Some(10),
                fields: vec![],
                key_candidates: vec![KeyCandidateProfile {
                    claim_id: "candidate_key:ANALYTICS.RAW.RAW_CARGO_BUILD_1_ORDERS:ID".to_string(),
                    field_names: vec!["ID".to_string()],
                    status: EvidenceStatus::Observed,
                    total_count: Some(10),
                    null_count: Some(0),
                    approx_distinct_count: Some(10),
                    distinct_count_exact: true,
                }],
                relationship_candidates: vec![],
            }],
            notes: vec![],
        };
        let task = model_task_with_spec(
            "fct_orders",
            vec!["stg_raw_cargo_build_1_orders"],
            Vec::new(),
            Vec::new(),
        );

        let refs = semantic_claim_refs_for_model_task(&profile, &task);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, SemanticClaimKind::CandidateKey);
        assert_eq!(refs[0].status, EvidenceStatus::Observed);
    }

    #[test]
    fn propagates_safe_claim_refs_through_intra_plan_inputs() {
        let observed_key = SemanticClaimRef {
            claim_id: "candidate_key:raw.orders:id".to_string(),
            kind: SemanticClaimKind::CandidateKey,
            status: EvidenceStatus::Observed,
        };
        let mut plan = crate::plan_types::ModelPlan {
            plan_key: "model-plan".to_string(),
            status: Default::default(),
            project_snapshot: Default::default(),
            tasks: vec![
                model_task_with_spec("fct_orders", vec!["stg_orders"], vec![observed_key], vec![]),
                model_task_with_spec(
                    "agg_orders",
                    vec!["fct_orders"],
                    vec![],
                    vec!["sum revenue"],
                ),
            ],
            batches: vec![vec!["fct_orders".to_string(), "agg_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: Default::default(),
        };

        let attached = attach_intra_plan_claim_refs(&mut plan);
        assert_eq!(attached, 1);
        let agg_refs = &plan.tasks[1]
            .implementation_spec
            .as_ref()
            .expect("spec")
            .evidence_claim_refs;
        assert_eq!(agg_refs.len(), 1);
        assert_eq!(agg_refs[0].kind, SemanticClaimKind::CandidateKey);
    }

    fn model_task_with_spec(
        name: &str,
        inputs: Vec<&str>,
        evidence_claim_refs: Vec<SemanticClaimRef>,
        metrics: Vec<&str>,
    ) -> crate::plan_types::ModelTask {
        crate::plan_types::ModelTask {
            name: name.to_string(),
            folder: Default::default(),
            goal: String::new(),
            inputs: inputs.iter().map(|input| input.to_string()).collect(),
            expected_model_path: None,
            invariants: vec![],
            implementation_spec: Some(crate::plan_types::ModelImplementationSpec {
                spec_version: 1,
                grain: "1 row per id".to_string(),
                inputs: inputs.iter().map(|input| input.to_string()).collect(),
                joins: vec![],
                metrics: metrics
                    .into_iter()
                    .map(|name| crate::plan_types::MetricSpec {
                        name: name.to_string(),
                        definition: format!("{name} metric"),
                        caveats: vec![],
                    })
                    .collect(),
                output_fields: vec![],
                assumptions: vec![],
                evidence_claim_refs,
            }),
            source_schema: vec![],
            grounded_inputs: vec![],
            status: Default::default(),
            checklist: vec![],
        }
    }
}
