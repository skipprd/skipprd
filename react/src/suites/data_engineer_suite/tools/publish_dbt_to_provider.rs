use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::cmp::Ordering;

use crate::agent::AgentCtx;
use crate::session::ThreadCacheStore;
use crate::tools::Tool;
use std::sync::Arc;
use crate::providers::{CatalogProvider, DatasetCatalogProvider};

pub struct PublishDbtToProviderTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
}

#[async_trait]
impl Tool for PublishDbtToProviderTool {
    fn name(&self) -> &'static str {
        "publish_dbt_to_provider"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let cfg = ctx
            .resolved_config
            .as_ref()
            .ok_or_else(|| "resolved_config missing (server must inject resolved YAML config into SuiteCtx)".to_string())?;

        let max_iters: usize = std::env::var("DBT_REPAIR_MAX_ITERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
            .min(25);

        // Enforce single active warehouse provider for publishing.
        let gen = crate::dbt::profile::generate_profiles_yml(cfg.as_ref())?;
        let provider_target = args
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| gen.target.clone());

        let confirm = args.get("confirm").and_then(|x| x.as_bool()).unwrap_or(false);
        let dataset_ids: Option<Vec<String>> = args
            .get("dataset_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        let dbt = ctx.dbt.as_ref().ok_or_else(|| "dbt provider missing".to_string())?;
        let query = ctx.query.as_ref();

        // Write generated profiles.yml into a temp dir and run compile.
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut p = PathBuf::from(td.path());
        p.push("profiles.yml");
        fs::write(&p, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        // Eagerly repair/refresh + compile until the project compiles cleanly (bounded).
        let (compile_res, compile_repair) = crate::dbt::repair_loop::run_repair_loop(
            ctx,
            dbt,
            &crate::providers::DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: Some(td.path().to_string_lossy().to_string()),
                target: provider_target.clone(),
                run: false,
                build: false,
            },
            max_iters,
            self.datasets.as_ref(),
            self.catalog.as_ref(),
            dataset_ids.as_deref(),
        )
        .await?;

        if !compile_res.ok || !compile_res.compile_ok {
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "compile",
                "result": compile_res,
                "dialect": crate::dbt::remediate::active_provider_dialect(cfg.as_ref()),
                "repair_report": compile_repair
            }));
        }

        // Fetch manifest.json from storage (uploaded by validate_project when compile succeeds)
        let base = ctx.keyspace.dbt_prefix(&ctx.scope);
        let base = base.trim_end_matches('/').to_string() + "/";
        let manifest_key = format!("{}target/manifest.json", base);
        let manifest_bytes = ctx
            .storage
            .get_bytes(&manifest_key)
            .await
            .map_err(|e| format!("failed to fetch manifest.json: {}", e))?;
        // NOTE: `manifest.json` bytes are not guaranteed stable across dbt runs (invocation ids, timestamps, ordering).
        // For approvals we use a deterministic "publish plan" hash derived from the manifest's model relations.
        let raw_manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));

        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("failed to parse manifest.json: {}", e))?;
        let relations = extract_relations(&manifest);
        let plan_sha256 = plan_sha256(&relations)?;

        // Determine last published digest (if any)
        let mut last_published_digest: Option<String> = None;
        let mut pending_plan_digest: Option<String> = None;
        let tid = ctx.thread_id.clone().unwrap_or_default();
        if !tid.is_empty() {
            if let Some(store) = ctx.thread_store.as_ref() {
                if let Some(log) = store.get(&tid).await {
                    for step in log.steps.iter().rev() {
                        if step.action != "publish_dbt_to_provider" {
                            continue;
                        }
                        let stage = step
                            .observation
                            .get("stage")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let d = step
                            .observation
                            .get("manifest_sha256")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if stage == "published" && last_published_digest.is_none() {
                            last_published_digest = d.clone();
                        }
                        if stage == "await_approval" && pending_plan_digest.is_none() {
                            pending_plan_digest = d;
                        }
                    }
                }
            }
        }

        if last_published_digest.as_deref() == Some(plan_sha256.as_str()) {
            return Ok(serde_json::json!({
                "ok": true,
                "stage": "no_change",
                // Back-compat key: historically was raw manifest sha; now it's a deterministic publish-plan sha.
                "manifest_sha256": plan_sha256,
                "raw_manifest_sha256": raw_manifest_sha256,
                "relations": relations,
                "dialect": crate::dbt::remediate::active_provider_dialect(cfg.as_ref()),
                "repair_report": compile_repair
            }));
        }

        // Always require approval before any dbt build/publish run.
        if !confirm {
            let exists = check_existing_relations(query, cfg.as_ref(), &relations).await;
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
                // Back-compat key: deterministic publish-plan sha.
                "manifest_sha256": plan_sha256,
                "raw_manifest_sha256": raw_manifest_sha256,
                "relations": relations,
                "exists": exists,
                "dialect": crate::dbt::remediate::active_provider_dialect(cfg.as_ref()),
                "repair_report": compile_repair
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

        // Run dbt build to publish, with eager repair until success (bounded).
        let (build_res, build_repair) = crate::dbt::repair_loop::run_repair_loop(
            ctx,
            dbt,
            &crate::providers::DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: Some(td.path().to_string_lossy().to_string()),
                target: provider_target.clone(),
                run: false,
                build: true,
            },
            max_iters,
            self.datasets.as_ref(),
            self.catalog.as_ref(),
            dataset_ids.as_deref(),
        )
        .await?;

        if !build_res.ok || build_res.run_ok == Some(false) {
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "build",
                "result": build_res,
                "dialect": crate::dbt::remediate::active_provider_dialect(cfg.as_ref()),
                "repair_report": build_repair
            }));
        }

        // Best-effort: cache published relation list in-memory for ask-mode prelude.
        if let Some(tid) = ctx.thread_id.as_deref() {
            if !tid.trim().is_empty() {
                let fqns = relations
                    .iter()
                    .map(|r| {
                        let db = if !r.schema.is_empty() { r.schema.clone() } else { r.database.clone() };
                        format!("{}.{}.{}", cfg.providers.athena.catalog, db, r.identifier)
                    })
                    .collect::<Vec<_>>();
                ThreadCacheStore::update_published(tid, &plan_sha256, fqns);
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "stage": "published",
            // Back-compat key: deterministic publish-plan sha.
            "manifest_sha256": plan_sha256,
            "raw_manifest_sha256": raw_manifest_sha256,
            "relations": relations,
            "dbt": build_res,
            "dialect": crate::dbt::remediate::active_provider_dialect(cfg.as_ref()),
            "repair_report": build_repair
        }))
    }
}

async fn check_existing_relations(
    query: Option<&std::sync::Arc<dyn crate::providers::QueryProvider>>,
    cfg: &crate::config::ReactResolvedConfig,
    relations: &[PublishedRelation],
) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::<String, bool>::new();
    let Some(q) = query else {
        // If we can't check existence, require approval (conservative).
        for r in relations {
            let db = if !r.schema.is_empty() {
                r.schema.clone()
            } else {
                r.database.clone()
            };
            let fqn = format!("{}.{}.{}", cfg.providers.athena.catalog, db, r.identifier);
            out.insert(fqn, true);
        }
        return out;
    };

    // Best-effort existence check via QueryProvider::schema (Athena uses Glue GetTable).
    for r in relations {
        // AthenaQueryProvider expects catalog.database.table.
        //
        // dbt-athena typically uses:
        // - `database`: catalog name (often AwsDataCatalog)
        // - `schema`: glue database
        // So we treat `schema` as the database for existence checks.
        let db = if !r.schema.is_empty() {
            r.schema.clone()
        } else {
            r.database.clone()
        };
        let fqn = format!("{}.{}.{}", cfg.providers.athena.catalog, db, r.identifier);
        let exists = q.schema(&fqn).await.is_ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::InMemoryStorageAdapter;
    use crate::agent::DefaultPolicy;
    use crate::providers::DbtProvider;
    use crate::providers::Keyspace;
    use crate::providers::RequestScope;
    use async_trait::async_trait;
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
        let cfg = crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: crate::providers::RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: None,
                    region: None,
                    result_s3: Some("s3://x/".to_string()),
                    source_database: None,
                    modeled_database: None,
                    catalog: "AwsDataCatalog".to_string(),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: false,
                    profiles_dir: None,
                    target: None,
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
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
        storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
        keyspace: Arc<dyn Keyspace>,
    }

    #[async_trait]
    impl DbtProvider for MockDbtProvider {
        async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> {
            Ok(())
        }

        async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> {
            Ok("k".to_string())
        }

        async fn write_metricflow_yaml(
            &self,
            _scope: &RequestScope,
            _dataset_id: &str,
            _name: &str,
            _yaml_text: &str,
        ) -> Result<String, String> {
            Ok("k".to_string())
        }

        async fn validate_project(&self, scope: &RequestScope, args: &crate::providers::DbtValidateArgs) -> Result<crate::providers::DbtValidateResult, String> {
            // When "compiling", simulate uploading a manifest.json where publish expects it.
            if !args.build {
                let base = self.keyspace.dbt_prefix(scope).trim_end_matches('/').to_string() + "/";
                let manifest_key = format!("{}target/manifest.json", base);
                let manifest = serde_json::json!({
                    "nodes": {
                        "model.pkg.m1": {"resource_type":"model","database":"AwsDataCatalog","schema":"picnic","alias":"m1","config":{"materialized":"view"}}
                    }
                });
                let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
                self.storage.put_bytes(&manifest_key, &bytes, "application/json").await?;
            }
            Ok(crate::providers::DbtValidateResult {
                ok: true,
                deps_ok: true,
                parse_ok: true,
                compile_ok: true,
                run_ok: Some(args.build),
                uploaded_target_files: 1,
                errors: vec![],
                warnings: vec![],
                logs: serde_json::json!({}),
            })
        }
    }

    #[tokio::test]
    async fn publish_always_requires_approval_before_build() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(crate::providers::keyspace::DefaultKeyspace::new("b".to_string()));
        let scope = crate::providers::RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
        let dbt: Arc<dyn DbtProvider> = Arc::new(MockDbtProvider { storage: storage.clone(), keyspace: keyspace.clone() });

        let cfg = Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: scope.clone(),
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: None,
                    region: None,
                    result_s3: Some("s3://x/".to_string()),
                    source_database: None,
                    modeled_database: Some("picnic".to_string()),
                    catalog: "AwsDataCatalog".to_string(),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: Some("athena".to_string()),
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        });

        let ctx = crate::agent::AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("model".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: crate::llm::create_llm(&crate::llm::LlmConfig::default()),
            storage,
            scope,
            keyspace,
            query: None,
            dbt: Some(dbt),
            vector: None,
            thread_store: None,
            resolved_config: Some(cfg),
        };

        let tool = PublishDbtToProviderTool { datasets: None, catalog: None };
        let obs = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(obs.get("stage").and_then(|v| v.as_str()), Some("await_approval"));
        assert_eq!(obs.get("await_approval").and_then(|v| v.as_bool()), Some(true));
    }
}

