use std::collections::{HashMap, HashSet};

use futures_util::stream::{self, StreamExt};
use react_core::session::{ThreadStep, ThreadStore, ToolObservation};
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build_all_with_progress(
        query: &dyn crate::providers::dataset_catalog_provider::DatasetCatalogProvider,
        dataset_ids: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>,
        thread: Option<(ThreadStore, String)>,
    ) -> Result<Vec<(String, super::types::DataCatalog)>, String> {
        fn dataset_label(ds_id: &str) -> String {
            ds_id.rsplit('.').next().unwrap_or(ds_id).to_string()
        }

        async fn append_step_retry(
            store: &ThreadStore,
            thread_id: &str,
            step: ThreadStep,
            what: &str,
        ) {
            // Best-effort durability: retries reduce orphaned start/end spans.
            // This is still non-fatal by design (preflight observability should not block progress).
            let max_attempts: usize = 3;
            for attempt in 1..=max_attempts {
                match store.append_step(thread_id, step.clone()).await {
                    Ok(_) => return,
                    Err(e) => {
                        warn!(
                            "ORCHESTRATOR: failed to persist preflight step what={} attempt={}/{} thread_id={} err={}",
                            what,
                            attempt,
                            max_attempts,
                            thread_id,
                            e
                        );
                        if attempt < max_attempts {
                            tokio::time::sleep(Duration::from_millis((attempt as u64) * 75)).await;
                        }
                    }
                }
            }
        }

        async fn tool_start(
            thread: &Option<(ThreadStore, String)>,
            tool_id: &str,
            name: &str,
            clean_name: &str,
            args: Value,
            payload: Option<Value>,
        ) {
            let Some((store, thread_id)) = thread.as_ref() else {
                return;
            };
            append_step_retry(
                store,
                thread_id,
                ThreadStep::ToolStart {
                    tool_id: tool_id.to_string(),
                    name: name.to_string(),
                    clean_name: clean_name.to_string(),
                    args,
                    status: "running".to_string(),
                    payload,
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "preflight".to_string(),
                },
                &format!("tool_start name={} tool_id={}", name, tool_id),
            )
            .await;
        }

        async fn tool_end(
            thread: &Option<(ThreadStore, String)>,
            tool_id: &str,
            name: &str,
            clean_name: &str,
            args: Value,
            ok: bool,
            payload: Option<Value>,
            error: Option<String>,
        ) {
            let Some((store, thread_id)) = thread.as_ref() else {
                return;
            };
            let status = if ok { "ok" } else { "failed" }.to_string();
            let raw_obs = if ok {
                serde_json::json!({"ok": true})
            } else {
                serde_json::json!({"ok": false, "errors": [error.clone().unwrap_or_else(|| "unknown error".to_string())]})
            };
            append_step_retry(
                store,
                thread_id,
                ThreadStep::ToolEnd {
                    tool_id: tool_id.to_string(),
                    name: name.to_string(),
                    clean_name: clean_name.to_string(),
                    args,
                    status,
                    payload,
                    observation: ToolObservation::normalize(raw_obs),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "preflight".to_string(),
                },
                &format!("tool_end name={} tool_id={}", name, tool_id),
            )
            .await;
        }
        let mut datasets = query.list_datasets().await?;
        if dataset_ids.is_empty() {
            info!("ORCHESTRATOR: building catalogs from provider dataset discovery (all datasets)");
        } else {
            let requested: HashSet<String> = dataset_ids.keys().cloned().collect();
            let mut found: HashSet<String> = HashSet::new();
            let mut filtered: Vec<crate::providers::dataset_catalog_provider::DatasetId> =
                Vec::new();
            for ds in datasets.into_iter() {
                let fqn = ds.fqn();
                if requested.contains(&fqn) {
                    found.insert(fqn);
                    filtered.push(ds);
                }
            }
            let mut missing: Vec<String> = requested.difference(&found).cloned().collect();
            missing.sort();
            info!(
                "ORCHESTRATOR: building catalogs from provided dataset_ids requested={} matched={} missing={}",
                requested.len(),
                filtered.len(),
                missing.len()
            );
            if !missing.is_empty() {
                let preview = missing.iter().take(20).cloned().collect::<Vec<_>>();
                warn!(
                    "ORCHESTRATOR: requested dataset_ids not found in provider discovery (showing up to 20 of {}): {:?}",
                    missing.len(),
                    preview
                );
            }
            datasets = filtered;
        }
        datasets.sort_by(|a, b| a.fqn().cmp(&b.fqn()));
        info!("ORCHESTRATOR: discovered {} dataset(s)", datasets.len());

        // Optional WS-visible preflight activity (tool_start/tool_end events).
        // Keep these coarse-grained to avoid flooding.
        let overall_tool_id = Uuid::new_v4().to_string();
        tool_start(
            &thread,
            &overall_tool_id,
            "preflight_catalog_all",
            "Build catalogs",
            serde_json::json!({"op":"build_all"}),
            Some(serde_json::json!({"datasets": datasets.len()})),
        )
        .await;

        // Canonical query concurrency is owned by the provider. Keep batching aligned so we don't
        // create unbounded in-flight work.
        let dataset_concurrency = query.max_concurrency().max(1);
        let stats_progress_every: usize = std::env::var("CATALOG_STATS_PROGRESS_EVERY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10)
            .max(1)
            .min(250);
        let max_fields: usize = 50;
        info!(
            "ORCHESTRATOR: dataset_concurrency={} stats_max_fields={} stats_progress_every={}",
            dataset_concurrency, max_fields, stats_progress_every
        );

        if let Some(p) = progress {
            p.start("Building stats");
        }
        let mut st = stream::iter(datasets.into_iter())
            .map(|ds| {
                let thread = thread.clone();
                async move {
                    let ds_id = ds.fqn();
                    let ds_label = dataset_label(&ds_id);

                    let dataset_tool_id = Uuid::new_v4().to_string();
                    tool_start(
                        &thread,
                        &dataset_tool_id,
                        "preflight_catalog_dataset",
                        &format!("Catalog {ds_label}"),
                        serde_json::json!({"dataset_id": ds_id}),
                        Some(serde_json::json!({"stage":"start"})),
                    )
                    .await;

                    info!("ORCHESTRATOR: dataset='{}' start", ds_id);

                    // Fetch schema once so we can embed types into the catalog fields.
                    let schema_tool_id = Uuid::new_v4().to_string();
                    tool_start(
                        &thread,
                        &schema_tool_id,
                        "preflight_catalog_schema",
                        &format!("Schema {ds_label}"),
                        serde_json::json!({"dataset_id": ds_id}),
                        None,
                    )
                    .await;

                    let schema_cols = match query.get_dataset_schema(&ds).await {
                        Ok(cols) => {
                            info!(
                                "ORCHESTRATOR: dataset='{}' schema_ok columns={}",
                                ds_id,
                                cols.len()
                            );
                            tool_end(
                                &thread,
                                &schema_tool_id,
                                "preflight_catalog_schema",
                                &format!("Schema {ds_label}"),
                                serde_json::json!({"dataset_id": ds_id}),
                                true,
                                Some(serde_json::json!({"columns": cols.len()})),
                                None,
                            )
                            .await;
                            cols
                        }
                        Err(e) => {
                            warn!(
                                "ORCHESTRATOR: dataset='{}' schema unavailable: {}",
                                ds_id, e
                            );
                            tool_end(
                                &thread,
                                &schema_tool_id,
                                "preflight_catalog_schema",
                                &format!("Schema {ds_label}"),
                                serde_json::json!({"dataset_id": ds_id}),
                                false,
                                None,
                                Some(e),
                            )
                            .await;
                            Vec::new()
                        }
                    };
                    let mut type_by_field: std::collections::HashMap<String, String> =
                        std::collections::HashMap::new();
                    let mut meta_by_field: std::collections::HashMap<
                        String,
                        (
                            String,
                            super::types::StructureKind,
                            super::types::AccessDescriptor,
                        ),
                    > = std::collections::HashMap::new();
                    for (name, ty) in schema_cols.iter() {
                        // Expand nested types into leaf dot-paths so catalog fields can carry types.
                        let flattened = crate::providers::type_parse::flatten_athena_type(name, ty);
                        for ff in flattened.into_iter() {
                            let path = ff.path.clone();
                            type_by_field.insert(path.clone(), ff.data_type.clone());
                            // Derive structure metadata. This is provider-agnostic at the representation layer
                            // (NestedPath vs ColumnName) even though we parsed engine-native types.
                            let is_nested = path.starts_with(&format!("{}.", name));
                            let kind = if ff.is_complex {
                                super::types::StructureKind::Complex
                            } else if is_nested {
                                super::types::StructureKind::NestedLeaf
                            } else {
                                super::types::StructureKind::Scalar
                            };
                            let access = if is_nested {
                                let rest = path.trim_start_matches(&format!("{}.", name));
                                let segs = rest
                                    .split('.')
                                    .filter(|s| !s.trim().is_empty())
                                    .map(|s| s.to_string())
                                    .collect::<Vec<_>>();
                                super::types::AccessDescriptor::NestedPath {
                                    root: name.clone(),
                                    path: segs,
                                }
                            } else {
                                super::types::AccessDescriptor::ColumnName { name: name.clone() }
                            };
                            meta_by_field.insert(path, (name.clone(), kind, access));
                        }
                    }

                    // Prefer provider stats. If unavailable, fall back to schema-only catalog.
                    info!(
                        "ORCHESTRATOR: dataset='{}' stats_start max_fields={}",
                        ds_id, max_fields
                    );
                    let stats_tool_id = Uuid::new_v4().to_string();
                    tool_start(
                        &thread,
                        &stats_tool_id,
                        "preflight_catalog_stats",
                        &format!("Stats {ds_label}"),
                        serde_json::json!({"dataset_id": ds_id, "max_fields": max_fields}),
                        None,
                    )
                    .await;
                    let (ns_stats_opt, ds_stats_opt) =
                        match query.get_dataset_stats(&ds, max_fields).await {
                            Ok((ns_stats, ds_stats)) => {
                                info!(
                            "ORCHESTRATOR: dataset='{}' stats_ok fields={} approx_total_rows={}",
                            ds_id,
                            ns_stats.fields.len(),
                            ds_stats.approx_total_rows
                        );
                                tool_end(
                            &thread,
                            &stats_tool_id,
                            "preflight_catalog_stats",
                            &format!("Stats {ds_label}"),
                            serde_json::json!({"dataset_id": ds_id, "max_fields": max_fields}),
                            true,
                            Some(serde_json::json!({
                                "fields": ns_stats.fields.len(),
                                "approx_total_rows": ds_stats.approx_total_rows
                            })),
                            None,
                        )
                        .await;
                                (Some(ns_stats), Some(ds_stats))
                            }
                            Err(e) => {
                                warn!("ORCHESTRATOR: dataset='{}' stats unavailable: {}", ds_id, e);
                                tool_end(
                            &thread,
                            &stats_tool_id,
                            "preflight_catalog_stats",
                            &format!("Stats {ds_label}"),
                            serde_json::json!({"dataset_id": ds_id, "max_fields": max_fields}),
                            false,
                            None,
                            Some(e),
                        )
                        .await;
                                (None, None)
                            }
                        };
                    let has_stats = ns_stats_opt.is_some();

                    // Ensure we can still build a usable catalog even without stats: seed fields from schema.
                    let ns_stats_seeded: Option<crate::discover::stats::DatasetFieldStats> =
                        if ns_stats_opt.is_some() {
                            ns_stats_opt
                        } else {
                            if schema_cols.is_empty() {
                                None
                            } else {
                                let mut ns = crate::discover::stats::DatasetFieldStats::new(&ds_id);
                                for (name, _ty) in schema_cols.iter() {
                                    for (path, _leaf_ty) in
                                        crate::providers::type_parse::flatten_type_paths(name, _ty)
                                            .into_iter()
                                    {
                                        ns.fields.entry(path).or_insert_with(
                                            crate::discover::stats::FieldStats::default,
                                        );
                                    }
                                }
                                Some(ns)
                            }
                        };

                    let mut cat = super::builder::CatalogBuilder::build_with_stats(
                        &ds,
                        ns_stats_seeded,
                        ds_stats_opt,
                    )
                    .await;
                    // Fill in per-field type strings (best-effort).
                    for f in cat.fields.iter_mut() {
                        if f.data_type.is_none() {
                            if let Some(ty) = type_by_field.get(&f.name) {
                                f.data_type = Some(ty.clone());
                            }
                        }
                        // Structure-aware metadata (best-effort).
                        if let Some((root, kind, access)) = meta_by_field.get(&f.name) {
                            if f.root_column.is_none() {
                                f.root_column = Some(root.clone());
                            }
                            if f.field_path.is_none() {
                                f.field_path = Some(f.name.clone());
                            }
                            if f.structure_kind.is_none() {
                                f.structure_kind = Some(kind.clone());
                            }
                            if f.access_descriptor.is_none() {
                                f.access_descriptor = Some(access.clone());
                            }
                        }
                    }
                    info!("ORCHESTRATOR: dataset='{}' catalog_built", ds_id);
                    let dataset_ok = !schema_cols.is_empty() || has_stats;
                    tool_end(
                        &thread,
                        &dataset_tool_id,
                        "preflight_catalog_dataset",
                        &format!("Catalog {ds_label}"),
                        serde_json::json!({"dataset_id": ds_id}),
                        dataset_ok,
                        Some(serde_json::json!({
                            "schema_columns": schema_cols.len(),
                            "catalog_fields": cat.fields.len(),
                            "has_stats": has_stats
                        })),
                        if dataset_ok {
                            None
                        } else {
                            Some("schema+stats unavailable".to_string())
                        },
                    )
                    .await;

                    (ds_id, cat)
                }
            })
            .buffer_unordered(dataset_concurrency);

        let mut to_write: Vec<(String, super::types::DataCatalog)> = Vec::new();
        while let Some(item) = st.next().await {
            to_write.push(item);
        }
        // Keep output deterministic for downstream writers.
        to_write.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(p) = progress {
            p.complete("Building stats");
        }

        if let Some(p) = progress {
            p.start("Building catalog");
        }
        // Avoid printing full catalog JSON at info-level: it’s very large and makes logs noisy.
        // Keep a small debug hint instead.
        for (ds_id, cat) in to_write.iter() {
            debug!(
                "ORCHESTRATOR: dataset='{}' catalog_summary fields={} structure_index_keys={}",
                ds_id,
                cat.fields.len(),
                cat.structure_index.len()
            );
        }
        if let Some(p) = progress {
            p.complete("Building catalog");
        }
        info!(
            "ORCHESTRATOR: completed building catalogs for {} dataset(s)",
            to_write.len()
        );

        tool_end(
            &thread,
            &overall_tool_id,
            "preflight_catalog_all",
            "Build catalogs",
            serde_json::json!({"op":"build_all"}),
            true,
            Some(serde_json::json!({"datasets": to_write.len()})),
            None,
        )
        .await;
        Ok(to_write)
    }

    // legacy wrapper removed
}
