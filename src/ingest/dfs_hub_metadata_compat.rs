//! Prove console bundled metadata matches DFS hub plugin fixture rows on fast-path ingest.
#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use skippr_plugin_data_source_dataforseo_seo_opportunities::config::{
        DataForSeoSeoOpportunitiesPluginConfig, LimitsConfig, RunMode,
    };
    use skippr_plugin_data_source_dataforseo_seo_opportunities::dataforseo_seo_opportunities::DataForSeoSeoOpportunitiesPlugin;
    use skippr_plugin_data_source_dataforseo_seo_opportunities::streams::{
        NAMESPACE_KEYWORD_METRIC_DAILY, NAMESPACE_KEYWORD_SUGGESTION_DAILY,
        NAMESPACE_OPPORTUNITY_SCORE_DAILY, NAMESPACE_SEED_KEYWORD_DAILY,
        NAMESPACE_SITE_RUN_DAILY,
    };
    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{
        DataSource, OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    use crate::discover::{Metadata, PipelineMetadata};
    use crate::ingest::fast_ingest::{
        create_default_nested_message, fast_path_ingest, DEFAULT_NESTED_MESSAGE,
    };
    use crate::ingest_work::{storage_namespace, Ingest};

    const FIXTURE_ENV: &str = "SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR";

    fn metadata_path() -> PathBuf {
        std::env::var("SKIPPR_DFS_HUB_METADATA_PATH").map_or_else(
            |_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../upfoundry/console-web/skippr/pipeline-metadata/dataforseo_seo_opportunities.json")
            },
            PathBuf::from,
        )
    }

    fn load_pipeline_metadata() -> PipelineMetadata {
        let raw = fs::read_to_string(metadata_path())
            .unwrap_or_else(|e| panic!("read {}: {e}", metadata_path().display()));
        serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parse pipeline metadata: {e}"))
    }

    fn hub_plugin_config() -> DataForSeoSeoOpportunitiesPluginConfig {
        DataForSeoSeoOpportunitiesPluginConfig {
            site: "skippr.io".into(),
            seed_keywords: vec!["meal planning app".into()],
            run_mode: RunMode::Mvp,
            streams: skippr_plugin_data_source_dataforseo_seo_opportunities::config::StreamKind::keyword_research_streams()
                .to_vec(),
            limits: LimitsConfig {
                max_generated_keywords: 3,
                serp_depth: 10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[derive(Default)]
    struct RecordingSyncContext {
        payloads: Mutex<HashMap<String, Vec<String>>>,
    }

    impl RecordingSyncContext {
        fn lines_for(&self, namespace: &str) -> Vec<Value> {
            self.payloads
                .lock()
                .unwrap()
                .get(namespace)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    impl SourceSyncContext for RecordingSyncContext {
        fn submit_payload_tasks(
            &self,
            tasks: Vec<SourcePayloadTask>,
        ) -> Result<ThroughputMetrics, std::io::Error> {
            for task in tasks {
                for batch in task.batches {
                    if let Some(ns) = batch.namespace {
                        if !batch.data.is_empty() {
                            self.payloads
                                .lock()
                                .unwrap()
                                .entry(ns)
                                .or_default()
                                .extend(batch.data.lines().map(str::to_string));
                        }
                    }
                }
            }
            Ok(ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: 0,
                queue_length: 0,
                optimal_chunk_size: 0,
            })
        }

        fn validate_offset_batch(
            &self,
            entries: &[OffsetValidationEntry],
        ) -> Result<Vec<bool>, std::io::Error> {
            Ok(vec![false; entries.len()])
        }

        fn relay_offset_hints(
            &self,
            _hints: Vec<RuntimeOffsetMaterializationHint>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        fn store_checkpoint(
            &self,
            _key: &str,
            _envelope: &CheckpointEnvelope,
        ) -> Result<(), String> {
            Ok(())
        }

        fn load_checkpoint_envelope(&self, _key: &str) -> Option<CheckpointEnvelope> {
            None
        }
    }

    fn inject_lineage_fields(mut row: Value) -> Value {
        if let Some(obj) = row.as_object_mut() {
            obj.insert(
                "workspace_id".into(),
                Value::String("fde8e559-4e04-423e-bb9c-29ca5605d465".into()),
            );
            obj.insert("domain_id".into(), Value::String("skippr.io".into()));
            obj.insert("domain".into(), Value::String("skippr.io".into()));
        }
        row
    }

    fn install_namespace_template(
        pipeline: &PipelineMetadata,
        plugin_namespace: &str,
    ) -> HashMap<String, Metadata> {
        let storage_ns = storage_namespace(plugin_namespace);
        let ns_meta = pipeline
            .metadata
            .get(&storage_ns)
            .unwrap_or_else(|| panic!("missing metadata namespace {storage_ns}"))
            .clone();
        let fields = ns_meta.fields.as_ref().clone();
        let template = create_default_nested_message(&fields);
        DEFAULT_NESTED_MESSAGE.insert(storage_ns.clone(), Arc::new(template));
        let _ = Ingest::prepare_arrow_schema_with_metadata(&storage_ns, &pipeline.metadata, false);
        fields
    }

    #[tokio::test]
    async fn dfs_hub_fixture_rows_match_console_metadata_fast_path() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/data_source/dataforseo_seo_opportunities/fixtures");
        std::env::set_var(FIXTURE_ENV, &fixture_dir);
        std::env::set_var("DATAFORSEO_API_USER", "fixture");
        std::env::set_var("DATAFORSEO_API_PASS", "fixture");

        let pipeline = load_pipeline_metadata();
        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(hub_plugin_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("fixture sync");

        let namespaces = [
            NAMESPACE_SEED_KEYWORD_DAILY,
            NAMESPACE_KEYWORD_SUGGESTION_DAILY,
            NAMESPACE_KEYWORD_METRIC_DAILY,
            NAMESPACE_OPPORTUNITY_SCORE_DAILY,
            NAMESPACE_SITE_RUN_DAILY,
        ];

        let mut failures = Vec::new();
        for plugin_ns in namespaces {
            let storage_ns = storage_namespace(plugin_ns);
            let fields = install_namespace_template(&pipeline, plugin_ns);
            let rows = ctx.lines_for(plugin_ns);
            assert!(
                !rows.is_empty(),
                "expected fixture rows for plugin namespace {plugin_ns}"
            );

            for (idx, row) in rows.into_iter().enumerate() {
                let row = inject_lineage_fields(row);
                match fast_path_ingest(&row, &fields, &storage_ns, false) {
                    Ok(_) => {}
                    Err(err) => failures.push(format!(
                        "ns={storage_ns} row={idx} err={err} row={row}"
                    )),
                }
            }
        }

        if !failures.is_empty() {
            panic!(
                "fast_path_ingest failures ({}):\n{}",
                failures.len(),
                failures.join("\n---\n")
            );
        }
    }

    #[test]
    fn dfs_hub_metadata_covers_plugin_emitted_fields() {
        let pipeline = load_pipeline_metadata();
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/data_source/dataforseo_seo_opportunities/fixtures");
        std::env::set_var(FIXTURE_ENV, &fixture_dir);
        std::env::set_var("DATAFORSEO_API_USER", "fixture");
        std::env::set_var("DATAFORSEO_API_PASS", "fixture");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut plugin = DataForSeoSeoOpportunitiesPlugin::new(hub_plugin_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("fixture sync");

        let plugin_namespaces = [
            NAMESPACE_KEYWORD_SUGGESTION_DAILY,
            NAMESPACE_KEYWORD_METRIC_DAILY,
            NAMESPACE_OPPORTUNITY_SCORE_DAILY,
            NAMESPACE_SEED_KEYWORD_DAILY,
        ];

        let mut missing = Vec::new();
        for plugin_ns in plugin_namespaces {
            let storage_ns = storage_namespace(plugin_ns);
            let meta_fields: HashSet<_> = pipeline
                .metadata
                .get(&storage_ns)
                .map(|m| m.fields.keys().cloned().collect())
                .unwrap_or_default();
            for row in ctx.lines_for(plugin_ns) {
                let row = inject_lineage_fields(row);
                let Some(obj) = row.as_object() else {
                    continue;
                };
                for key in obj.keys() {
                    if !meta_fields.contains(key) {
                        missing.push(format!("{storage_ns}: field '{key}' missing from metadata"));
                    }
                }
            }
        }

        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            panic!("plugin fields missing from metadata:\n{}", missing.join("\n"));
        }
    }
}
