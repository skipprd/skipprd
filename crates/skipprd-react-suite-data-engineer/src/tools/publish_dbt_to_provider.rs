use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::progress_controller::ExecutionState;
use crate::providers::{CatalogProvider, DatasetCatalogProvider};
use crate::state_manager;
use crate::thread_cache::ThreadCacheStore;
use react_core::agent::AgentCtx;
use react_core::resolved_config::ReactResolvedConfig;
use react_core::tools::Tool;
use std::sync::Arc;

pub struct PublishDbtToProviderTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
}

use super::model_authoring_engine::emit_trace;

#[async_trait]
impl Tool for PublishDbtToProviderTool {
    fn name(&self) -> &'static str {
        "publish_dbt_to_provider"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        emit_trace(ctx, "publish started");
        let cfg = resolved_config(ctx)?;

        // Enforce single active warehouse provider for publishing.
        let threads = crate::ctx_ext::actx_query(ctx).map(|q| q.max_concurrency());
        let gen = crate::dbt::profile::generate_profiles_yml(cfg, threads)?;
        let provider_target = args
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| gen.target.clone());

        let confirm = args
            .get("confirm")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let _dataset_ids: Option<Vec<String>> = args
            .get("dataset_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        let dbt =
            crate::ctx_ext::actx_dbt(ctx).ok_or_else(|| "dbt provider missing".to_string())?;
        let query = crate::ctx_ext::actx_query(ctx);

        // Write generated profiles.yml into a temp dir and run compile.
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut p = PathBuf::from(td.path());
        p.push("profiles.yml");
        fs::write(&p, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        let compile_args = crate::providers::DbtValidateArgs {
            project_name: crate::env_util::SUITE_PROJECT_NAME.to_string(),
            profiles_dir: Some(td.path().to_string_lossy().to_string()),
            target: provider_target.clone(),
            run: false,
            build: false,
            select: None,
            exclude: None,
        };
        let compile_res =
            crate::transient_retry::retry_transient_default("publish_compile", || async {
                dbt.validate_project(ctx.scope(), &compile_args).await
            })
            .await?;

        if !compile_res.ok || !compile_res.compile_ok {
            emit_trace(ctx, "publish failed");
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "compile",
                "result": compile_res,
                "dialect": crate::dialect::active_provider_dialect(cfg)
            }));
        }

        // Fetch manifest.json from storage (uploaded by validate_project when compile succeeds)
        let base = ctx.keyspace().scoped_prefix(ctx.scope(), &["dbt"]);
        let base = base.trim_end_matches('/').to_string() + "/";
        let manifest_key = format!("{}target/manifest.json", base);
        let manifest_bytes = ctx
            .storage()
            .get_bytes(&manifest_key)
            .await
            .map_err(|e| format!("failed to fetch manifest.json: {}", e))?;
        // NOTE: `manifest.json` bytes are not guaranteed stable across dbt runs (invocation ids, timestamps, ordering).
        // For approvals we use a deterministic "publish plan" hash derived from the manifest's model relations.

        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("failed to parse manifest.json: {}", e))?;
        let relations = extract_relations(&manifest);
        let plan_sha256 = plan_sha256(&relations)?;

        // Determine last published digest (if any) from typed execution state.
        let mut last_published_digest: Option<String> = None;
        let mut pending_plan_digest: Option<String> = None;
        let tid = ctx.thread_id().clone().unwrap_or_default();
        if !tid.is_empty() {
            if let Some(store) = ctx.thread_store().as_ref() {
                let es = state_manager::load_execution_state_strict(&store.control_store(), &tid)
                    .await
                    .map_err(|e| format!("failed to load strict execution state for publish: {e}"))?
                    .unwrap_or_else(ExecutionState::new);
                if let crate::progress_controller::PublishStatus::Succeeded { ref plan_sha256 } =
                    es.publish
                {
                    last_published_digest = Some(plan_sha256.clone());
                }
                pending_plan_digest = es.publish.plan_sha256().map(|s| s.to_string());
            }
        }

        if last_published_digest.as_deref() == Some(plan_sha256.as_str()) {
            emit_trace(ctx, "publish no change");
            return Ok(serde_json::json!({
                "ok": true,
                "stage": "no_change",
                "plan_sha256": plan_sha256,
                "relations": relations,
                "dialect": crate::dialect::active_provider_dialect(cfg)
            }));
        }

        // Always require approval before any dbt build/publish run.
        if !confirm {
            if !tid.is_empty() {
                if let Some(store) = ctx.thread_store().as_ref() {
                    state_manager::apply_execution_event(
                        &store.control_store(),
                        &tid,
                        crate::progress_controller::DataEngineerEvent::PublishPlanPending {
                            sha256: plan_sha256.clone(),
                        },
                    )
                    .await
                    .map_err(|e| format!("failed to persist pending_publish_plan: {e}"))?;
                }
            }
            emit_trace(ctx, "publish awaiting approval");
            let exists = check_existing_relations(query.as_ref(), cfg, &relations).await;
            let existing_list = exists
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>();
            let risk_note = if query.is_none() {
                "NOTE: QueryProvider is not configured; relation existence could not be verified. Treat this as overwrite-risk.\n\n"
            } else if existing_list.is_empty() {
                ""
            } else {
                "WARNING: Some relations appear to already exist and may be overwritten.\n\n"
            };
            let prompt = format!(
                "{}Approve to run dbt build (publish) in the warehouse?\n\nIf approved: re-run publish with confirm=true.\n\nPlan: {}\nExisting: {}",
                risk_note,
                plan_sha256,
                if existing_list.is_empty() { "(none detected)".to_string() } else { existing_list.join(", ") }
            );
            return Ok(serde_json::json!({
                "ok": true,
                "stage": "await_approval",
                "await_approval": true,
                "prompt": prompt,
                "plan_sha256": plan_sha256,
                "relations": relations,
                "exists": exists,
                "dialect": crate::dialect::active_provider_dialect(cfg)
            }));
        } else {
            // confirm=true: ensure we have a matching pending plan (best-effort safety)
            if let Some(p) = pending_plan_digest.as_deref() {
                if p != plan_sha256 {
                    return Err(
                        "confirm=true but pending publish plan does not match current plan digest; rerun without confirm"
                            .to_string(),
                    );
                }
            }
        }

        let build_args = crate::providers::DbtValidateArgs {
            project_name: crate::env_util::SUITE_PROJECT_NAME.to_string(),
            profiles_dir: Some(td.path().to_string_lossy().to_string()),
            target: provider_target.clone(),
            run: false,
            build: true,
            select: None,
            exclude: None,
        };
        let build_res =
            crate::transient_retry::retry_transient_default("publish_build", || async {
                dbt.validate_project(ctx.scope(), &build_args).await
            })
            .await?;

        if !build_res.ok || build_res.run_ok == Some(false) {
            emit_trace(ctx, "publish failed");
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "build",
                "result": build_res,
                "dialect": crate::dialect::active_provider_dialect(cfg)
            }));
        }

        // Best-effort: cache published relation list in-memory for ask-mode prelude.
        if let Some(tid) = ctx.thread_id().as_deref() {
            if !tid.trim().is_empty() {
                let providers = crate::de_config::de_config_from_resolved(cfg);
                let wh_container = providers
                    .as_ref()
                    .map(|p| p.warehouse.container.as_str())
                    .unwrap_or("");
                let fqns = relations
                    .iter()
                    .map(|r| {
                        let db = if !r.schema.is_empty() {
                            r.schema.clone()
                        } else {
                            r.database.clone()
                        };
                        format!("{}.{}.{}", wh_container, db, r.identifier)
                    })
                    .collect::<Vec<_>>();
                ThreadCacheStore::update_published(tid, &plan_sha256, fqns);
            }
        }

        emit_trace(ctx, "publish finished");
        if !tid.is_empty() {
            if let Some(store) = ctx.thread_store().as_ref() {
                state_manager::apply_execution_event(
                    &store.control_store(),
                    &tid,
                    crate::progress_controller::DataEngineerEvent::PublishCompleted {
                        sha256: plan_sha256.clone(),
                    },
                )
                .await
                .map_err(|e| format!("failed to persist mark_publish_complete: {e}"))?;
            }
        }
        Ok(serde_json::json!({
            "ok": true,
            "stage": "published",
            "plan_sha256": plan_sha256,
            "relations": relations,
            "dbt": build_res,
            "dialect": crate::dialect::active_provider_dialect(cfg)
        }))
    }
}

async fn check_existing_relations(
    query: Option<&std::sync::Arc<dyn crate::providers::QueryProvider>>,
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    relations: &[PublishedRelation],
) -> BTreeMap<String, bool> {
    let providers = crate::de_config::de_config_from_resolved(cfg);
    let wh_container = providers
        .as_ref()
        .map(|p| p.warehouse.container.as_str())
        .unwrap_or("");
    let mut out = BTreeMap::<String, bool>::new();
    let Some(q) = query else {
        // If we can't check existence, require approval (conservative).
        for r in relations {
            let db = if !r.schema.is_empty() {
                r.schema.clone()
            } else {
                r.database.clone()
            };
            let fqn = format!("{}.{}.{}", wh_container, db, r.identifier);
            out.insert(fqn, true);
        }
        return out;
    };

    // Best-effort existence check via QueryProvider::schema.
    for r in relations {
        let db = if !r.schema.is_empty() {
            r.schema.clone()
        } else {
            r.database.clone()
        };
        let fqn = format!("{}.{}.{}", wh_container, db, r.identifier);
        let exists =
            crate::transient_retry::retry_transient_default("publish_check_schema", || async {
                q.schema(&fqn).await
            })
            .await
            .is_ok();
        out.insert(fqn, exists);
    }
    out
}

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    #[serde(default)]
    nodes: BTreeMap<String, ManifestNode>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestNode {
    #[serde(default)]
    resource_type: String,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    config: ManifestNodeConfig,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ManifestNodeConfig {
    #[serde(default)]
    materialized: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishedRelation {
    pub database: String,
    pub schema: String,
    pub identifier: String,
    pub materialized: String,
}

fn plan_sha256(relations: &[PublishedRelation]) -> Result<String, String> {
    // Canonicalize (sort) so the resulting hash is deterministic across dbt runs.
    let mut v = relations.to_vec();
    v.sort_by(|a, b| {
        let k1 = (&a.database, &a.schema, &a.identifier, &a.materialized);
        let k2 = (&b.database, &b.schema, &b.identifier, &b.materialized);
        match k1.0.cmp(k2.0) {
            Ordering::Equal => match k1.1.cmp(k2.1) {
                Ordering::Equal => match k1.2.cmp(k2.2) {
                    Ordering::Equal => k1.3.cmp(k2.3),
                    o => o,
                },
                o => o,
            },
            o => o,
        }
    });
    let bytes = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

fn extract_relations(m: &Manifest) -> Vec<PublishedRelation> {
    let mut out: Vec<PublishedRelation> = Vec::new();
    for (_k, n) in m.nodes.iter() {
        if n.resource_type != "model" {
            continue;
        }
        let db = n.database.clone().unwrap_or_default();
        let schema = n.schema.clone().unwrap_or_default();
        let ident = n.alias.clone().unwrap_or_default();
        if db.is_empty() || schema.is_empty() || ident.is_empty() {
            continue;
        }
        let mat = n
            .config
            .materialized
            .clone()
            .unwrap_or_else(|| "view".to_string());
        out.push(PublishedRelation {
            database: db,
            schema,
            identifier: ident,
            materialized: mat,
        });
    }
    out
}

fn resolved_config(ctx: &AgentCtx) -> Result<&ReactResolvedConfig, String> {
    ctx.resolved_config()
        .as_ref()
        .map(|c| c.as_ref())
        .ok_or_else(|| "resolved_config missing (server must inject resolved YAML config into AgentCtx.resolved_config())".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::DbtProvider;
    use async_trait::async_trait;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[test]
    fn parse_manifest_extracts_models() {
        let j = r#"{"nodes": {"model.x.y": {"resource_type":"model","database":"db","schema":"sc","alias":"m1","config":{"materialized":"view"}}}}"#;
        let m: Manifest = serde_json::from_str(j).unwrap();
        let rels = extract_relations(&m);
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].identifier, "m1");
        assert_eq!(rels[0].materialized, "view");
    }

    #[tokio::test]
    async fn check_existing_relations_is_conservative_without_query_provider() {
        let cfg = react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "src", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": false, "target": "athena", "naming": {}, "runner": "host" },
                "vector": { "enabled": false }
            }),
        };
        let rels = vec![PublishedRelation {
            database: "AwsDataCatalog".to_string(),
            schema: "my_db".to_string(),
            identifier: "my_table".to_string(),
            materialized: "view".to_string(),
        }];
        let m = check_existing_relations(None, &cfg, &rels).await;
        let key = "AwsDataCatalog.my_db.my_table".to_string();
        assert_eq!(m.get(&key).copied(), Some(true));
    }

    struct MockDbtProvider {
        storage: Arc<dyn StorageAdapter>,
        keyspace: Arc<dyn Keyspace>,
    }

    #[async_trait]
    impl DbtProvider for MockDbtProvider {
        async fn ensure_minimal_project(
            &self,
            _scope: &RequestScope,
        ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String> {
            Ok(vec![])
        }

        async fn write_model_sql(
            &self,
            _scope: &RequestScope,
            _rel_path: &str,
            _sql: &str,
        ) -> Result<String, String> {
            Ok("k".to_string())
        }

        async fn write_metricflow_yaml(
            &self,
            _scope: &RequestScope,
            _rel_path: &str,
            _yaml_text: &str,
        ) -> Result<String, String> {
            Ok("k".to_string())
        }

        async fn validate_project(
            &self,
            scope: &RequestScope,
            args: &crate::providers::DbtValidateArgs,
        ) -> Result<crate::providers::DbtValidateResult, String> {
            // When "compiling", simulate uploading a manifest.json where publish expects it.
            if !args.build {
                let base = self
                    .keyspace
                    .scoped_prefix(scope, &["dbt"])
                    .trim_end_matches('/')
                    .to_string()
                    + "/";
                let manifest_key = format!("{}target/manifest.json", base);
                let manifest = serde_json::json!({
                    "nodes": {
                        "model.pkg.m1": {"resource_type":"model","database":"AwsDataCatalog","schema":"picnic","alias":"m1","config":{"materialized":"view"}}
                    }
                });
                let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
                self.storage
                    .put_bytes(&manifest_key, &bytes, "application/json")
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(crate::providers::DbtValidateResult {
                ok: true,
                deps_ok: true,
                parse_ok: true,
                compile_ok: true,
                run_ok: Some(args.build),
                uploaded_target_files: 1,
                failure_class: crate::failure_kind::FailureKind::Unknown,
                errors: vec![],
                warnings: vec![],
                logs: serde_json::json!({}),
                stripped: vec![],
            })
        }
    }

    #[tokio::test]
    async fn publish_always_requires_approval_before_build() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let dbt: Arc<dyn DbtProvider> = Arc::new(MockDbtProvider {
            storage: storage.clone(),
            keyspace: keyspace.clone(),
        });

        let cfg = Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: scope.clone(),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "picnic", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "picnic", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        });

        let warehouse: Arc<dyn crate::providers::WarehouseProvider> =
            Arc::new(crate::providers::warehouse::NullWarehouseProvider::default());
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(react_core::llm::NullModel::new()),
            storage,
            scope,
            keyspace,
            Arc::new(DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("tid".to_string())
        .agent_name("model".to_string())
        .resolved_config(Some(cfg.clone()))
        .build();
        ctx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        ctx.set_capability(Arc::new(crate::ctx_ext::DbtCap(dbt)));

        let tool = PublishDbtToProviderTool {
            datasets: None,
            catalog: None,
        };
        let obs = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(
            obs.get("stage").and_then(|v| v.as_str()),
            Some("await_approval")
        );
        assert_eq!(
            obs.get("await_approval").and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}
