use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CatalogBootstrapOutcome {
    pub(super) metadata_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum BootstrapStatus {
    Ready,
    BestEffort,
    Pending,
}

impl BootstrapStatus {
    fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::BestEffort)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct CatalogBootstrapCache {
    status: BootstrapStatus,
    metadata_complete: bool,
    ts: String,
}

impl Default for BootstrapStatus {
    fn default() -> Self {
        Self::Pending
    }
}

const BOOTSTRAP_SUITE_ID: &str = "data_engineer_bootstrap";

impl DataEngineerSuite {
    pub(super) async fn ensure_catalog_bootstrap_semaphored(
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<(), String> {
        let control = sctx.control_store();
        if let Ok(Some(cache)) = control
            .load::<CatalogBootstrapCache>(thread_id, BOOTSTRAP_SUITE_ID)
            .await
        {
            if cache.status.is_usable() {
                tracing::info!(
                    "data_engineer: catalog bootstrap semaphore hit status={:?} thread_id={}",
                    cache.status,
                    thread_id
                );
                return Ok(());
            }
        }
        let bootstrap_timeout_secs = env_util::catalog_bootstrap_timeout_secs();
        let out = match tokio::time::timeout(
            std::time::Duration::from_secs(bootstrap_timeout_secs),
            Self::ensure_catalog_bootstrap(sctx),
        )
        .await
        {
            Ok(v) => v?,
            Err(_) => {
                tracing::warn!(
                    "data_engineer: catalog bootstrap timed out after {}s; continuing best-effort",
                    bootstrap_timeout_secs
                );
                CatalogBootstrapOutcome {
                    metadata_complete: false,
                }
            }
        };
        let status = if out.metadata_complete {
            BootstrapStatus::Ready
        } else {
            BootstrapStatus::BestEffort
        };
        let ts = chrono::Utc::now().to_rfc3339();
        let cache = CatalogBootstrapCache {
            status,
            metadata_complete: out.metadata_complete,
            ts,
        };
        if let Err(e) = control.save(thread_id, BOOTSTRAP_SUITE_ID, &cache).await {
            tracing::warn!(
                "data_engineer: failed to persist catalog bootstrap state thread_id={} err={}",
                thread_id,
                e
            );
        }
        Ok(())
    }

    /// Hard cutover: refresh canonical catalog state on every run.
    ///
    // TODO(item-99): ensure_catalog_bootstrap is ~220 lines. Extract sub-functions for:
    // (a) provider capability resolution, (b) catalog refresh dispatch, (c) metadata gate
    // checks (schemas, tables, sample data), (d) outcome assembly.
    /// This builds/refreshes warehouse-backed catalog artifacts and then enforces that required
    /// metadata exists so planning can treat catalog as canonical.
    pub(super) async fn ensure_catalog_bootstrap(
        sctx: &SuiteCtx,
    ) -> Result<CatalogBootstrapOutcome, String> {
        let (Some(cat), Some(datasets)) = (
            crate::ctx_ext::sctx_catalog(sctx),
            crate::ctx_ext::sctx_datasets(sctx),
        ) else {
            return Ok(CatalogBootstrapOutcome {
                metadata_complete: true,
            });
        };
        let dss = datasets.list_datasets().await.map_err(|e| {
            tracing::error!(
                error = %e,
                "data_engineer: catalog bootstrap dataset discovery failed"
            );
            format!("catalog bootstrap failed: dataset discovery error: {e}")
        })?;
        if dss.is_empty() {
            return Err("catalog bootstrap failed: dataset discovery returned zero datasets. The configured warehouse schema appears empty; run `skippr sync` for this pipeline before `skippr model`, then verify the data sink database/schema in skippr.yml if the problem persists.".to_string());
        }

        // Also detect whether the global semantic context exists.
        let global_key = sctx.keyspace().scoped_key(
            sctx.scope(),
            &[
                "semantic",
                &format!(
                    "{}.yaml",
                    encode_key_component(crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
                ),
            ],
        );
        tracing::info!(
            "data_engineer: refreshing canonical catalogs/stats for {} dataset(s)",
            dss.len()
        );
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        cat.build_all_with_progress(sctx.scope(), datasets.as_ref(), &empty, None)
            .await
            .map_err(|e| format!("catalog bootstrap failed while building catalogs: {e}"))?;

        // Mandatory metadata completion pass.
        let all: std::collections::HashSet<String> = dss.iter().map(|ds| ds.fqn()).collect();
        crate::semantic_profile::build_and_store_semantic_profiles(
            cat.as_ref(),
            sctx.scope(),
            &all,
        )
        .await
        .map_err(|e| format!("catalog bootstrap failed while building semantic profiles: {e}"))?;

        let pipeline = crate::ctx_ext::sctx_pipeline(sctx)?;
        match crate::lineage_builder::refresh_lineage_graph_for_suite(
            sctx,
            crate::lineage_builder::LineageBuildOptions {
                pipeline,
                include_query_history: false,
                query_history_since: None,
                query_history_limit: 100,
            },
        )
        .await
        {
            Ok(result) => tracing::info!(
                "data_engineer: lineage graph refreshed nodes={} edges={} diagnostics={}",
                result.node_count,
                result.edge_count,
                result.diagnostic_count
            ),
            Err(e) => tracing::warn!(
                "data_engineer: lineage graph refresh failed during catalog bootstrap; continuing: {}",
                e
            ),
        }

        let enrich_report = if bootstrap_catalog_llm_enrichment_enabled() {
            let enrich_timeout_secs = enrichment_timeout_secs(all.len());
            match tokio::time::timeout(
                std::time::Duration::from_secs(enrich_timeout_secs),
                cat.run_llm_enrichment_all(sctx.scope(), &all),
            )
            .await
            {
                Ok(Ok(report)) => report,
                Ok(Err(e)) => {
                    tracing::warn!(
                        "data_engineer: catalog LLM enrichment failed; continuing with deterministic metadata repair: {}",
                        e
                    );
                    crate::providers::CatalogEnrichmentReport {
                        dataset_total: all.len(),
                        dataset_enriched_failed: all.len(),
                        global_context_error: Some(e),
                        ..Default::default()
                    }
                }
                Err(_) => {
                    let err = format!("timed out after {}s", enrich_timeout_secs);
                    tracing::warn!(
                        "data_engineer: catalog LLM enrichment {}; continuing with deterministic metadata repair",
                        err
                    );
                    crate::providers::CatalogEnrichmentReport {
                        dataset_total: all.len(),
                        dataset_enriched_failed: all.len(),
                        global_context_error: Some(err),
                        ..Default::default()
                    }
                }
            }
        } else {
            tracing::info!(
                "data_engineer: skipping catalog LLM enrichment during bootstrap; deterministic metadata repair remains enabled"
            );
            crate::providers::CatalogEnrichmentReport {
                dataset_total: all.len(),
                ..Default::default()
            }
        };
        tracing::info!(
            "data_engineer: catalog enrichment summary datasets={} ok={} failed={} global_written={}",
            enrich_report.dataset_total,
            enrich_report.dataset_enriched_ok,
            enrich_report.dataset_enriched_failed,
            enrich_report.global_context_written
        );

        // Single metadata gate + single deterministic repair attempt.
        // If metadata still doesn't fully converge, continue to planning with warnings.
        let collect_meta_errors = || async {
            let mut errs: Vec<String> = Vec::new();
            for ds in dss.iter() {
                let id = ds.fqn();
                let Some(c) = cat.read_catalog(sctx.scope(), &id).await.map_err(|e| {
                    format!("catalog bootstrap failed while reading catalog for {id}: {e}")
                })?
                else {
                    errs.push(format!("{id}: catalog missing after refresh"));
                    continue;
                };
                if c.description
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    errs.push(format!("{id}: missing dataset description"));
                }
                let missing_fields = c
                    .fields
                    .iter()
                    .filter(|f| {
                        f.description
                            .as_deref()
                            .map(|s| s.trim().is_empty())
                            .unwrap_or(true)
                    })
                    .count();
                if missing_fields > 0 {
                    errs.push(format!(
                        "{id}: {} field(s) missing field descriptions",
                        missing_fields
                    ));
                }
            }
            let gctx = sctx
                .storage()
                .get_json(&global_key)
                .await
                .ok()
                .and_then(|v| {
                    serde_json::from_value::<crate::providers::GlobalSemanticContext>(v).ok()
                });
            match gctx {
                Some(g) => {
                    if g.audiences.is_empty() {
                        errs.push("global_semantic_context: audiences is empty".to_string());
                    }
                    if g.context_bullets.is_empty() {
                        errs.push("global_semantic_context: context_bullets is empty".to_string());
                    }
                }
                None => errs.push("global_semantic_context: missing".to_string()),
            }
            Ok::<Vec<String>, String>(errs)
        };

        let mut meta_errors = collect_meta_errors().await?;
        if !meta_errors.is_empty() {
            tracing::warn!(
                "data_engineer: catalog metadata gate failed; applying single deterministic repair attempt:\n- {}",
                meta_errors.join("\n- ")
            );
            // Deterministic catalog description repair.
            for ds in dss.iter() {
                let id = ds.fqn();
                let Some(mut c) = cat.read_catalog(sctx.scope(), &id).await.map_err(|e| {
                    format!("catalog bootstrap failed while reading catalog for {id}: {e}")
                })?
                else {
                    continue;
                };
                let mut changed = false;
                if c.description
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    let field_preview = c
                        .fields
                        .iter()
                        .map(|f| f.name.clone())
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(", ");
                    c.description = Some(if field_preview.is_empty() {
                        format!(
                            "Dataset {} contains source records used for analytics modeling.",
                            id
                        )
                    } else {
                        format!(
                            "Dataset {} contains source records with fields {} for analytics modeling.",
                            id, field_preview
                        )
                    });
                    changed = true;
                }
                for f in c.fields.iter_mut() {
                    if f.description
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                    {
                        f.description =
                            Some(format!("Field {} in dataset {}.", f.name, c.dataset_id));
                        changed = true;
                    }
                }
                if changed {
                    cat.write_catalog(sctx.scope(), &id, &c)
                        .await
                        .map_err(|e| format!("catalog bootstrap failed while writing {id}: {e}"))?;
                }
            }
            // Deterministic global semantic context repair.
            let gctx = sctx
                .storage()
                .get_json(&global_key)
                .await
                .ok()
                .and_then(|v| {
                    serde_json::from_value::<crate::providers::GlobalSemanticContext>(v).ok()
                });
            let needs_global_defaults = match gctx {
                Some(ref g) => g.audiences.is_empty() || g.context_bullets.is_empty(),
                None => true,
            };
            if needs_global_defaults {
                let dataset_ids = dss.iter().map(|d| d.fqn()).collect::<Vec<_>>();
                let default_global = crate::providers::GlobalSemanticContext {
                    version: 1,
                    built_at_epoch_secs: Some((chrono::Utc::now().timestamp()).max(0) as u64),
                    audiences: vec![crate::providers::GlobalAudience {
                        audience: "Analytics engineering and data consumers".to_string(),
                        confidence: 0.90,
                        evidence: dataset_ids
                            .iter()
                            .take(5)
                            .map(|d| format!("dataset_id={}", d))
                            .collect(),
                    }],
                    context_bullets: vec![crate::providers::GlobalContextBullet {
                        text: "Project models warehouse datasets for analytics use-cases."
                            .to_string(),
                        confidence: 0.90,
                        evidence: dataset_ids
                            .iter()
                            .take(5)
                            .map(|d| format!("dataset_id={}", d))
                            .collect(),
                    }],
                    dataset_groups: vec![],
                    assumptions_and_gaps: vec![],
                };
                let value = serde_json::to_value(default_global)
                    .map_err(|e| format!("catalog bootstrap failed while encoding global semantic context defaults: {e}"))?;
                sctx.storage()
                    .put_json(&global_key, &value)
                    .await
                    .map_err(|e| format!("catalog bootstrap failed while writing global semantic context defaults: {e}"))?;
            }
            meta_errors = collect_meta_errors().await?;
            if !meta_errors.is_empty() {
                tracing::warn!(
                    "data_engineer: catalog metadata still incomplete after single repair attempt; proceeding to planning with defaults best-effort:\n- {}",
                    meta_errors.join("\n- ")
                );
            }
        }
        Ok(CatalogBootstrapOutcome {
            metadata_complete: meta_errors.is_empty(),
        })
    }
}

fn enrichment_timeout_secs(dataset_count: usize) -> u64 {
    ((dataset_count.max(1) as u64) * 15).clamp(30, 120)
}

fn bootstrap_catalog_llm_enrichment_enabled() -> bool {
    std::env::var(env_util::env_keys::DE_CATALOG_LLM_ENRICHMENT)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::enrichment_timeout_secs;

    #[test]
    fn enrichment_timeout_is_bounded_by_dataset_count() {
        assert_eq!(enrichment_timeout_secs(0), 30);
        assert_eq!(enrichment_timeout_secs(2), 30);
        assert_eq!(enrichment_timeout_secs(4), 60);
        assert_eq!(enrichment_timeout_secs(100), 120);
    }
}
