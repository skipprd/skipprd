use react_core::agent::AgentCtx;
use react_core::storage::retry_get_bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SqlDialect(pub String);

/// How broad the auto-attached schema facts should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactsScope {
    /// Plan mode: intentionally broad so planning has enough context to avoid guessing.
    PlanBroad,
    /// Author mode: include all relations involved in the current approved batch.
    AuthorBatch,
    /// Validate failure: minimal set derived from the failing model(s) + deps.
    ValidateFail,
}

impl fmt::Display for FactsScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FactsScope::PlanBroad => write!(f, "plan_broad"),
            FactsScope::AuthorBatch => write!(f, "author_batch"),
            FactsScope::ValidateFail => write!(f, "validate_fail"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FactsLimits {
    /// Maximum number of relations to include.
    #[serde(default)]
    pub max_relations: usize,
    /// Maximum number of columns per relation.
    #[serde(default)]
    pub max_columns_per_relation: usize,
}

impl FactsLimits {
    pub fn for_scope(scope: FactsScope) -> Self {
        match scope {
            // Broad, but bounded.
            FactsScope::PlanBroad => Self {
                max_relations: 60,
                max_columns_per_relation: 250,
            },
            // Cover the batch; still bounded.
            FactsScope::AuthorBatch => Self {
                max_relations: 30,
                max_columns_per_relation: 250,
            },
            // Minimal.
            FactsScope::ValidateFail => Self {
                max_relations: 20,
                max_columns_per_relation: 250,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnFact {
    pub name: String,
    #[serde(default)]
    pub r#type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelationFacts {
    /// Fully-qualified name, e.g. AwsDataCatalog.test_silver.stg_x
    pub fqn: String,
    #[serde(default)]
    pub columns: Vec<ColumnFact>,
    /// Where these facts came from (e.g. provider/catalog).
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactsBundle {
    pub scope: FactsScope,
    pub dialect: SqlDialect,
    #[serde(default)]
    pub relations: Vec<RelationFacts>,
    /// Canonical tool contracts.
    pub tool_contracts: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ManifestIndexes {
    pub models: BTreeMap<String, (String, String)>,
    pub sources: BTreeMap<(String, String), String>,
}

fn file_patch_contract_value() -> Value {
    // Keep this strictly mechanical and JSON-only. This is intended to be pasted into prompts
    // as an immutable contract, and reused in deterministic validators.
    serde_json::json!({
        "file": {
            "ops": {
                "patch": {
                    "top_level_args": {
                        "op": "patch",
                        "path": "string (single target file; required)",
                        "patch_text": "string (Cursor/Aider hunks-only unified diff; starts with '@@'; no file headers)"
                    },
                    "rules": [
                        "args.op MUST equal \"patch\"",
                        "Provide patch_text only (other patch primitives are not allowed)",
                        "args.path MUST be provided and patch_text MUST target exactly that one path",
                        "patch_text MUST start with '@@' and MUST NOT include git file headers (---/+++), diff --git preamble, or diffy-style headers (--- original / +++ modified)",
                        "Hunk headers MUST be Cursor/Aider style: '@@ ... @@' (no line numbers; never '@@ -a,b +c,d @@')"
                    ],
                    "example": serde_json::from_str::<Value>(
                        crate::patch_contract::single_file_patch_good_example_json()
                    ).unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "malformed embedded patch-contract JSON; using inline fallback");
                        serde_json::json!({
                            "op": "patch",
                            "path": "models/staging/stg_example.sql",
                            "patch_text": "@@ ... @@\n- old\n+ new\n"
                        })
                    })
                },
                "rm": {
                    "top_level_args": {
                        "op": "rm",
                        "path": "string (project-relative path)",
                        "expected_sha256": "optional string (only checked if the file exists)"
                    },
                    "rules": [
                        "args.op MUST equal \"rm\"",
                        "args.path MUST be a project-relative path (no absolute paths, no '..')",
                        "If expected_sha256 is provided and the file exists, it MUST match the current file content sha256"
                    ],
                    "example": {
                        "action": "file",
                        "args": {"op":"rm","path":"models/staging/staging.sql"}
                    }
                },
                "mv": {
                    "top_level_args": {
                        "op": "mv",
                        "from": "string (project-relative path)",
                        "to": "string (project-relative path)",
                        "expected_sha256": "optional string (checked against the source file content)"
                    },
                    "rules": [
                        "args.op MUST equal \"mv\"",
                        "args.from and args.to MUST be project-relative paths (no absolute paths, no '..')",
                        "Destination MUST NOT already exist (no implicit overwrite)",
                        "If expected_sha256 is provided, it MUST match the current source file sha256"
                    ],
                    "example": {
                        "action": "file",
                        "args": {"op":"mv","from":"models/staging/foo.sql","to":"models/staging/stg_test_raw_raw_customers.sql"}
                    }
                }
            }
        }
    })
}

async fn schema_columns_for_fqn(ctx: &AgentCtx, fqn: &str) -> Option<RelationFacts> {
    let q = crate::ctx_ext::actx_query(ctx)?;
    match q.schema(fqn).await {
        Ok(cols) => {
            let columns: Vec<ColumnFact> = cols
                .into_iter()
                .map(|(n, t)| ColumnFact { name: n, r#type: t })
                .collect();
            Some(RelationFacts {
                fqn: fqn.to_string(),
                columns,
                source: "provider".to_string(),
            })
        }
        Err(_) => None,
    }
}

async fn load_manifest_value(ctx: &AgentCtx) -> Option<Value> {
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string()
        + "/";
    let key = format!("{}target/manifest.json", base);
    let bytes = retry_get_bytes(ctx.storage().as_ref(), &key).await.ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn build_manifest_model_index(v: &Value) -> BTreeMap<String, (String, String)> {
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();
    let Some(nodes) = v.get("nodes").and_then(|n| n.as_object()) else {
        return out;
    };
    for (_uid, node) in nodes.iter() {
        let rt = node
            .get("resource_type")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if rt != "model" {
            continue;
        }
        let name = node
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        let database = node
            .get("database")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let schema = node
            .get("schema")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let alias = node
            .get("alias")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let file_path = node
            .get("original_file_path")
            .and_then(|x| x.as_str())
            .or_else(|| node.get("path").and_then(|x| x.as_str()))
            .unwrap_or("")
            .trim();
        if database.is_empty() || schema.is_empty() || alias.is_empty() || file_path.is_empty() {
            continue;
        }
        let fqn = format!("{}.{}.{}", database, schema, alias);
        out.insert(name.to_string(), (fqn, file_path.to_string()));
    }
    out
}

fn build_manifest_source_index(v: &Value) -> BTreeMap<(String, String), String> {
    let mut out: BTreeMap<(String, String), String> = BTreeMap::new();
    let Some(sources) = v.get("sources").and_then(|n| n.as_object()) else {
        return out;
    };
    for (_uid, node) in sources.iter() {
        let source_name = node
            .get("source_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let table_name = node
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let database = node
            .get("database")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let schema = node
            .get("schema")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let identifier = node
            .get("identifier")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        if source_name.is_empty()
            || table_name.is_empty()
            || database.is_empty()
            || schema.is_empty()
            || identifier.is_empty()
        {
            continue;
        }
        let fqn = format!("{}.{}.{}", database, schema, identifier);
        out.insert((source_name.to_string(), table_name.to_string()), fqn);
    }
    out
}

/// Build minimal manifest indexes from one best-effort manifest read.
pub async fn load_manifest_indexes(ctx: &AgentCtx) -> ManifestIndexes {
    let Some(v) = load_manifest_value(ctx).await else {
        return ManifestIndexes::default();
    };
    ManifestIndexes {
        models: build_manifest_model_index(&v),
        sources: build_manifest_source_index(&v),
    }
}

/// Build a minimal manifest index mapping model name -> (fqn, original_file_path).
pub async fn load_manifest_index(ctx: &AgentCtx) -> BTreeMap<String, (String, String)> {
    load_manifest_indexes(ctx).await.models
}

/// Build a minimal manifest index mapping (source_name, table_name) -> fqn.
pub async fn load_manifest_source_index(ctx: &AgentCtx) -> BTreeMap<(String, String), String> {
    load_manifest_indexes(ctx).await.sources
}

/// Build facts for a deterministic dbt_validate failure observation.
///
/// Returns dialect + tool_contracts only (no model/column extraction).
pub fn build_validate_fail_facts(
    _ctx: &AgentCtx,
    dialect: SqlDialect,
    _validate_contract: &crate::controller_event::ValidateObservationContract,
    scope: FactsScope,
) -> FactsBundle {
    FactsBundle {
        scope,
        dialect,
        relations: Vec::new(),
        tool_contracts: file_patch_contract_value(),
    }
}

/// Build an auto-attached FactsBundle given a set of relation FQNs to include.
pub async fn build_facts_bundle_from_relations(
    ctx: &AgentCtx,
    scope: FactsScope,
    dialect: SqlDialect,
    relation_fqns: &[String],
    limits: FactsLimits,
) -> FactsBundle {
    let mut rels: Vec<RelationFacts> = Vec::new();
    for fqn in relation_fqns.iter().take(limits.max_relations.max(1)) {
        if let Some(mut r) = schema_columns_for_fqn(ctx, fqn).await {
            if limits.max_columns_per_relation > 0
                && r.columns.len() > limits.max_columns_per_relation
            {
                r.columns.truncate(limits.max_columns_per_relation);
            }
            rels.push(r);
        }
    }
    FactsBundle {
        scope,
        dialect,
        relations: rels,
        tool_contracts: file_patch_contract_value(),
    }
}

/// Best-effort helper to resolve a list of model names into relation FQNs using manifest.json.
pub async fn resolve_model_names_to_fqns(ctx: &AgentCtx, model_names: &[String]) -> Vec<String> {
    let idx = load_manifest_indexes(ctx).await.models;
    resolve_model_names_to_fqns_from_index(&idx, model_names)
}

pub fn resolve_model_names_to_fqns_from_index(
    idx: &BTreeMap<String, (String, String)>,
    model_names: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in model_names.iter() {
        if let Some((fqn, _file)) = idx.get(n) {
            out.push(fqn.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Resolve raw dataset_ids (catalog.schema.table) into relation FQNs (identical).
pub fn dataset_ids_to_fqns(dataset_ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = dataset_ids
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Resolve live warehouse schema for a relation into plan-compatible column defs.
/// Returns `None` when the warehouse is unreachable or the relation has no columns.
pub(crate) async fn resolve_source_schema_live(
    query: &dyn crate::providers::QueryProvider,
    relation_fqn: &str,
) -> Option<Vec<crate::plan_types::SourceColumnDef>> {
    match query.schema(relation_fqn).await {
        Ok(cols) if !cols.is_empty() => Some(
            cols.into_iter()
                .map(|(name, data_type)| crate::plan_types::SourceColumnDef { name, data_type })
                .collect(),
        ),
        _ => None,
    }
}

/// Small helper to merge, sort, and cap relation lists deterministically.
#[cfg(test)]
pub fn merge_relation_fqns(mut a: Vec<String>, b: Vec<String>) -> Vec<String> {
    for it in b {
        a.push(it);
    }
    a.sort();
    a.dedup();
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::de_config;
    use crate::providers::{DbtProvider, QueryProvider};
    use async_trait::async_trait;
    use react_core::agent::{AgentCtx, DefaultPolicy};
    use react_core::error::CoreError;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::NullModel;
    use react_core::resolved_config as config;
    use react_core::scope::RequestScope;
    use react_core::storage::{cached, ConditionalWriteStatus, StorageAdapter};
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    fn minimal_cfg() -> Arc<config::ReactResolvedConfig> {
        Arc::new(config::ReactResolvedConfig {
            server: config::ServerResolved { port: 1 },
            storage: config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: config::LlmResolved::default(),
            suite_config: serde_json::json!({}),
        })
    }

    fn minimal_providers() -> de_config::ProvidersResolved {
        de_config::ProvidersResolved {
            warehouse: de_config::WarehouseResolved {
                kind: de_config::WarehouseKind::Athena,
                container: "AwsDataCatalog".to_string(),
                namespace: "test_raw".to_string(),
                extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
            },
            catalog: de_config::CatalogResolved {
                enabled: false,
                refresh_secs: 60,
                max_concurrency: 8,
            },
            dbt: de_config::DbtResolved {
                enabled: true,
                profiles_dir: None,
                target: "athena".to_string(),
                naming: de_config::DbtNamingResolved {
                    target_schema: "test".to_string(),
                    silver_suffix: "silver".to_string(),
                    gold_suffix: "gold".to_string(),
                },
                runner: "host".to_string(),
                docker_image: None,
                docker_platform: None,
                docker_network: None,
                docker_mount_aws_dir: false,
            },
            vector: de_config::VectorResolved { enabled: false },
            el: de_config::ElToolResolved::default(),
        }
    }

    struct MockQueryProvider;
    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn query(&self, _sql: &str) -> Result<crate::providers::QueryResult, String> {
            Err("not implemented".to_string())
        }
        async fn schema(&self, table: &str) -> Result<Vec<(String, String)>, String> {
            if table == "AwsDataCatalog.test_silver.stg_dep" {
                return Ok(vec![
                    ("session_id".to_string(), "varchar".to_string()),
                    ("event_timestamp".to_string(), "timestamp".to_string()),
                ]);
            }
            Err("unknown table".to_string())
        }
        async fn sample(&self, _table: &str, _k: usize) -> Result<Vec<Vec<String>>, String> {
            Err("not implemented".to_string())
        }
        fn max_concurrency(&self) -> usize {
            1
        }
    }

    struct NoopDbtProvider;
    #[async_trait]
    impl DbtProvider for NoopDbtProvider {
        async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> {
            Ok(())
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
            _scope: &RequestScope,
            _args: &crate::providers::DbtValidateArgs,
        ) -> Result<crate::providers::DbtValidateResult, String> {
            Ok(crate::providers::DbtValidateResult::default())
        }
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>, query: Arc<dyn QueryProvider>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(NullModel::new()),
            storage,
            scope,
            keyspace,
            Arc::new(DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        actx.set_capability(Arc::new(crate::ctx_ext::QueryCap(query)));
        actx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(Arc::new(
            crate::providers::warehouse::NullWarehouseProvider::default(),
        ))));
        actx.set_capability(Arc::new(crate::ctx_ext::DbtCap(Arc::new(NoopDbtProvider))));
        actx.set_capability(Arc::new(crate::ctx_ext::ProvidersCfgCap(
            minimal_providers(),
        )));
        actx
    }

    #[derive(Default)]
    struct CountingStorageAdapter {
        bytes: Mutex<HashMap<String, Vec<u8>>>,
        get_bytes_calls: AtomicUsize,
    }

    impl CountingStorageAdapter {
        fn seed_bytes(&self, key: &str, bytes: &[u8]) {
            self.bytes
                .lock()
                .expect("bytes lock")
                .insert(key.to_string(), bytes.to_vec());
        }

        fn get_bytes_calls(&self) -> usize {
            self.get_bytes_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl StorageAdapter for CountingStorageAdapter {
        async fn get_json(&self, key: &str) -> Result<Value, CoreError> {
            let bytes = self.get_bytes(key).await?;
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|e| CoreError::Storage(format!("get_json('{key}'): {e}")))
        }

        async fn put_json(&self, key: &str, value: &Value) -> Result<(), CoreError> {
            let bytes = serde_json::to_vec(value)
                .map_err(|e| CoreError::Storage(format!("put_json('{key}'): {e}")))?;
            self.put_bytes(key, &bytes, "application/json").await
        }

        async fn put_json_if_etag_matches(
            &self,
            key: &str,
            value: &Value,
            _expected_etag: Option<&str>,
        ) -> Result<ConditionalWriteStatus, CoreError> {
            self.put_json(key, value).await?;
            Ok(ConditionalWriteStatus::Written)
        }

        async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, CoreError> {
            self.get_bytes_calls.fetch_add(1, Ordering::SeqCst);
            self.bytes
                .lock()
                .expect("bytes lock")
                .get(key)
                .cloned()
                .ok_or_else(|| CoreError::Storage(format!("get_bytes('{key}'): not found")))
        }

        async fn put_bytes(
            &self,
            key: &str,
            bytes: &[u8],
            _content_type: &str,
        ) -> Result<(), CoreError> {
            self.bytes
                .lock()
                .expect("bytes lock")
                .insert(key.to_string(), bytes.to_vec());
            Ok(())
        }

        async fn delete_object(&self, key: &str) -> Result<(), CoreError> {
            self.bytes.lock().expect("bytes lock").remove(key);
            Ok(())
        }

        async fn head_etag(&self, key: &str) -> Result<Option<String>, CoreError> {
            Ok(self
                .bytes
                .lock()
                .expect("bytes lock")
                .contains_key(key)
                .then(|| "etag".to_string()))
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, CoreError> {
            let mut keys: Vec<String> = self
                .bytes
                .lock()
                .expect("bytes lock")
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect();
            keys.sort();
            Ok(keys)
        }
    }

    #[test]
    fn extract_source_calls_finds_pairs() {
        let sql = "select 1 from {{ source('raw','events') }} join {{source(\"raw\",\"users\")}} u on 1=1";
        let got = crate::naming::extract_source_calls(sql);
        assert_eq!(
            got,
            vec![
                ("raw".to_string(), "events".to_string()),
                ("raw".to_string(), "users".to_string())
            ]
        );
    }

    #[test]
    fn build_validate_fail_facts_returns_dialect_and_tool_contracts() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let query: Arc<dyn QueryProvider> = Arc::new(MockQueryProvider);
        let ctx = make_ctx(storage.clone(), query);

        let validate_obs = serde_json::json!({
            "errors": [],
            "logs": {}
        });
        let contract = crate::controller_event::validate_contract_from_observation(validate_obs)
            .expect("validate contract");
        let facts = build_validate_fail_facts(
            &ctx,
            SqlDialect("Amazon Athena (engine v3 / Trino SQL)".to_string()),
            &contract,
            FactsScope::ValidateFail,
        );

        assert_eq!(facts.scope, FactsScope::ValidateFail);
        assert_eq!(facts.dialect.0, "Amazon Athena (engine v3 / Trino SQL)");
        assert!(facts.relations.is_empty());
        assert!(facts.tool_contracts.get("file").is_some());
    }

    #[tokio::test]
    async fn manifest_indexes_share_one_cached_storage_read() {
        let inner = Arc::new(CountingStorageAdapter::default());
        let manifest_key = "t/w/p/dbt/target/manifest.json";
        inner.seed_bytes(
            manifest_key,
            serde_json::json!({
                "nodes": {
                    "model.proj.stg_events": {
                        "resource_type": "model",
                        "name": "stg_events",
                        "database": "AwsDataCatalog",
                        "schema": "test_silver",
                        "alias": "stg_events",
                        "original_file_path": "models/staging/stg_events.sql"
                    }
                },
                "sources": {
                    "source.proj.raw.events": {
                        "source_name": "raw",
                        "name": "events",
                        "database": "AwsDataCatalog",
                        "schema": "test_raw",
                        "identifier": "events"
                    }
                }
            })
            .to_string()
            .as_bytes(),
        );
        let storage = cached(inner.clone());
        let query: Arc<dyn QueryProvider> = Arc::new(MockQueryProvider);
        let ctx = make_ctx(storage, query);

        let indexes = load_manifest_indexes(&ctx).await;
        assert_eq!(
            indexes.models.get("stg_events"),
            Some(&(
                "AwsDataCatalog.test_silver.stg_events".to_string(),
                "models/staging/stg_events.sql".to_string()
            ))
        );
        assert_eq!(
            indexes
                .sources
                .get(&("raw".to_string(), "events".to_string())),
            Some(&"AwsDataCatalog.test_raw.events".to_string())
        );

        let _ = load_manifest_index(&ctx).await;
        let _ = load_manifest_source_index(&ctx).await;
        assert_eq!(inner.get_bytes_calls(), 1);
    }
}
