use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::agent::AgentCtx;
use crate::session::ThreadCacheStore;
use crate::tools::Tool;

pub struct PublishDbtToProviderTool;

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

        // Enforce single active warehouse provider for publishing.
        let gen = crate::dbt::profile::generate_profiles_yml(cfg.as_ref())?;
        let provider_target = args
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| gen.target.clone());

        let confirm = args.get("confirm").and_then(|x| x.as_bool()).unwrap_or(false);

        let dbt = ctx.dbt.as_ref().ok_or_else(|| "dbt provider missing".to_string())?;
        let query = ctx.query.as_ref();

        // Write generated profiles.yml into a temp dir and run compile.
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut p = PathBuf::from(td.path());
        p.push("profiles.yml");
        fs::write(&p, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        let compile_res = dbt
            .validate_project(
                &ctx.scope,
                &crate::providers::DbtValidateArgs {
                    project_name: "data_engineer".to_string(),
                    profiles_dir: Some(td.path().to_string_lossy().to_string()),
                    target: provider_target.clone(),
                    run: false,
                    build: false,
                },
            )
            .await?;

        if !compile_res.ok || !compile_res.compile_ok {
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "compile",
                "result": compile_res
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
        let digest = format!("{:x}", Sha256::digest(&manifest_bytes));

        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("failed to parse manifest.json: {}", e))?;
        let relations = extract_relations(&manifest);

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

        if last_published_digest.as_deref() == Some(digest.as_str()) {
            return Ok(serde_json::json!({
                "ok": true,
                "stage": "no_change",
                "manifest_sha256": digest,
                "relations": relations
            }));
        }

        // If we are not confirming yet, check for existence and request approval if overwrite risk.
        if !confirm {
            let exists = check_existing_relations(query, cfg.as_ref(), &relations).await;
            let any_exists = exists.values().any(|v| *v);
            if any_exists {
                let prompt = format!(
                    "{}Publish will overwrite existing relation(s) in the warehouse. Approve to proceed?\n\nIf approved: re-run publish with confirm=true.\n\nManifest: {}\nExisting: {}",
                    if query.is_none() {
                        "NOTE: QueryProvider is not configured; relation existence could not be verified. Approval is required.\n\n"
                    } else {
                        ""
                    },
                    digest,
                    exists.iter().filter(|(_,v)| **v).map(|(k,_)| k.clone()).collect::<Vec<_>>().join(", ")
                );
                return Ok(serde_json::json!({
                    "ok": true,
                    "stage": "await_approval",
                    "await_approval": true,
                    "prompt": prompt,
                    "manifest_sha256": digest,
                    "relations": relations,
                    "exists": exists
                }));
            }
            // No existing relations → safe to publish without approval.
        } else {
            // confirm=true: ensure we have a matching pending plan (best-effort safety)
            if let Some(p) = pending_plan_digest.as_deref() {
                if p != digest {
                    return Err("confirm=true but pending publish plan does not match current manifest digest; rerun without confirm".to_string());
                }
            }
        }

        // Run dbt build to publish.
        let build_res = dbt
            .validate_project(
                &ctx.scope,
                &crate::providers::DbtValidateArgs {
                    project_name: "data_engineer".to_string(),
                    profiles_dir: Some(td.path().to_string_lossy().to_string()),
                    target: provider_target.clone(),
                    run: false,
                    build: true,
                },
            )
            .await?;

        if !build_res.ok || build_res.run_ok == Some(false) {
            return Ok(serde_json::json!({
                "ok": false,
                "stage": "build",
                "result": build_res
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
                ThreadCacheStore::update_published(tid, &digest, fqns);
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "stage": "published",
            "manifest_sha256": digest,
            "relations": relations,
            "dbt": build_res
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
                    result_s3: Some("s3://x/".to_string()),
                    default_database: None,
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
}

