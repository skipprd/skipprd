use react_core::agent::AgentCtx;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// How broad the auto-attached schema facts should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TargetFacts {
    #[serde(default)]
    pub failing_models: Vec<Value>,
    #[serde(default)]
    pub missing_columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactsBundle {
    pub scope: String,
    pub dialect: String,
    #[serde(default)]
    pub targets: TargetFacts,
    #[serde(default)]
    pub relations: Vec<RelationFacts>,
    /// Canonical tool contracts.
    pub tool_contracts: Value,
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
                        "Provide patch_text only (legacy patch primitives are not allowed)",
                        "args.path MUST be provided and patch_text MUST target exactly that one path",
                        "patch_text MUST start with '@@' and MUST NOT include git file headers (---/+++), diff --git preamble, or diffy-style headers (--- original / +++ modified)",
                        "Hunk headers MUST be Cursor/Aider style: '@@ ... @@' (no line numbers; never '@@ -a,b +c,d @@')"
                    ],
                    "example": serde_json::from_str::<Value>(
                        crate::data_engineer::patch_contract::single_file_patch_good_example_json()
                    ).unwrap_or_else(|_| serde_json::json!({
                        "op": "patch",
                        "path": "models/staging/stg_example.sql",
                        "patch_text": "@@ ... @@\n- old\n+ new\n"
                    }))
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

pub fn extract_missing_columns_from_dbt_errors(errors: &[String]) -> Vec<String> {
    // Best-effort parse of lines like:
    //   Column 'session_id_from_event' cannot be resolved
    let mut out: BTreeSet<String> = BTreeSet::new();
    for e in errors.iter() {
        let s = e.as_str();
        let needle = "Column '";
        let mut idx = 0usize;
        while let Some(pos) = s[idx..].find(needle) {
            let start = idx + pos + needle.len();
            if let Some(end_rel) = s[start..].find("' cannot be resolved") {
                let col = s[start..start + end_rel].trim();
                if !col.is_empty() {
                    out.insert(col.to_string());
                }
                idx = start + end_rel + 1;
            } else {
                break;
            }
        }
    }
    out.into_iter().collect()
}

fn dbt_storage_key(ctx: &AgentCtx, rel_path: &str) -> String {
    let base = ctx.keyspace.dbt_prefix(&ctx.scope);
    let base = base.trim_end_matches('/').to_string() + "/";
    format!("{}{}", base, rel_path.trim_start_matches('/'))
}

fn parse_quoted(s: &str, mut i: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let q = bytes[i] as char;
    if q != '\'' && q != '"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == q {
            let out = s[start..i].to_string();
            return Some((out, i + 1));
        }
        // minimal escape handling
        if c == '\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        i += 1;
    }
    None
}

/// Extract dbt `source('src','table')` calls from SQL/Jinja.
pub fn extract_source_calls(sql: &str) -> Vec<(String, String)> {
    let s = sql.to_ascii_lowercase();
    let mut idx = 0usize;
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some(pos) = s[idx..].find("source(") {
        let mut j = idx + pos + "source(".len();
        // parse first string
        let Some((a, j2)) = parse_quoted(&s, j) else {
            idx = j;
            continue;
        };
        j = j2;
        // seek comma
        if let Some(comma) = s[j..].find(',') {
            j = j + comma + 1;
        } else {
            idx = j;
            continue;
        }
        let Some((b, j3)) = parse_quoted(&s, j) else {
            idx = j;
            continue;
        };
        let a = a.trim().to_string();
        let b = b.trim().to_string();
        if !a.is_empty() && !b.is_empty() {
            out.insert((a, b));
        }
        idx = j3;
    }
    out.into_iter().collect()
}

async fn schema_columns_for_fqn(ctx: &AgentCtx, fqn: &str) -> Option<RelationFacts> {
    let q = ctx.query.as_ref()?;
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

/// Build a minimal manifest index mapping model name -> (fqn, original_file_path).
pub async fn load_manifest_index(ctx: &AgentCtx) -> BTreeMap<String, (String, String)> {
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string()
        + "/";
    let key = format!("{}target/manifest.json", base);
    let Ok(bytes) = ctx.storage.get_bytes(&key).await else {
        return out;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return out;
    };
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

/// Build a minimal manifest index mapping (source_name, table_name) -> fqn.
pub async fn load_manifest_source_index(ctx: &AgentCtx) -> BTreeMap<(String, String), String> {
    let mut out: BTreeMap<(String, String), String> = BTreeMap::new();
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string()
        + "/";
    let key = format!("{}target/manifest.json", base);
    let Ok(bytes) = ctx.storage.get_bytes(&key).await else {
        return out;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return out;
    };
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

async fn read_dbt_file_text(ctx: &AgentCtx, rel_path: &str, max_bytes: usize) -> Option<String> {
    let key = dbt_storage_key(ctx, rel_path);
    let Ok(bytes) = ctx.storage.get_bytes(&key).await else {
        return None;
    };
    let s = String::from_utf8_lossy(&bytes).to_string();
    if max_bytes > 0 && s.len() > max_bytes {
        Some(s[..max_bytes].to_string())
    } else {
        Some(s)
    }
}

/// Build facts for a deterministic dbt_validate failure observation.
///
/// This is intended to be attached to `validate_fail` reason_detail and injected into the next
/// authoring prompt so the LLM cannot guess relation schemas or columns.
pub async fn build_validate_fail_facts(
    ctx: &AgentCtx,
    dialect: String,
    validate_contract: &crate::data_engineer::controller_event::ValidateObservationContract,
    scope: FactsScope,
    limits: FactsLimits,
) -> FactsBundle {
    let validate_obs = &validate_contract.observation;
    // Extract errors (array-of-strings shape).
    let errs: Vec<String> = validate_obs
        .get("errors")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let logs_v = validate_obs.get("logs").cloned().unwrap_or(Value::Null);
    let failing_models = crate::data_engineer::dbt_error::extract_failed_models_from_logs(&logs_v);
    let missing_columns = crate::data_engineer::dbt_error::extract_unresolved_columns(&errs);

    // Collect dependency relations from failing model SQL: ref() and source().
    let mut want_ref_names: BTreeSet<String> = BTreeSet::new();
    let mut want_sources: BTreeSet<(String, String)> = BTreeSet::new();
    for fm in failing_models.iter() {
        let Some(file) = fm.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(sql) = read_dbt_file_text(ctx, file, 200_000).await else {
            continue;
        };
        for r in crate::data_engineer::naming::extract_ref_calls(&sql) {
            want_ref_names.insert(r);
        }
        for (sname, tname) in extract_source_calls(&sql) {
            want_sources.insert((sname, tname));
        }
    }

    // Resolve deps to concrete relation FQNs using manifest indexes when possible.
    let model_idx = load_manifest_index(ctx).await;
    let source_idx = load_manifest_source_index(ctx).await;
    let mut relation_fqns: Vec<String> = Vec::new();
    for r in want_ref_names.into_iter() {
        if let Some((fqn, _file)) = model_idx.get(&r) {
            relation_fqns.push(fqn.clone());
        }
    }
    for (sname, tname) in want_sources.into_iter() {
        if let Some(fqn) = source_idx.get(&(sname, tname)) {
            relation_fqns.push(fqn.clone());
        }
    }
    relation_fqns.sort();
    relation_fqns.dedup();

    let targets = TargetFacts {
        failing_models,
        missing_columns: if !missing_columns.is_empty() {
            missing_columns
        } else {
            extract_missing_columns_from_dbt_errors(&errs)
        },
    };
    build_facts_bundle_from_relations(ctx, scope, dialect, targets, &relation_fqns, limits).await
}

/// Build an auto-attached FactsBundle given a set of relation FQNs to include.
pub async fn build_facts_bundle_from_relations(
    ctx: &AgentCtx,
    scope: FactsScope,
    dialect: String,
    targets: TargetFacts,
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
        scope: scope.to_string(),
        dialect,
        targets,
        relations: rels,
        tool_contracts: file_patch_contract_value(),
    }
}

/// Best-effort helper to resolve a list of model names into relation FQNs using manifest.json.
pub async fn resolve_model_names_to_fqns(ctx: &AgentCtx, model_names: &[String]) -> Vec<String> {
    let idx = load_manifest_index(ctx).await;
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

/// Small helper to merge, sort, and cap relation lists deterministically.
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
    use crate::config;
    use async_trait::async_trait;
    use react_core::agent::{AgentCtx, DefaultPolicy};
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::NullModel;
    use react_core::providers::{DbtProvider, QueryProvider};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::Arc;

    fn minimal_cfg() -> Arc<config::ReactResolvedConfig> {
        Arc::new(config::ReactResolvedConfig {
            server: config::ServerResolved { port: 1 },
            storage: config::StorageResolved { mode: react_core::resolved_config::StorageMode::Local, bucket: None, path: None },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: config::LlmResolved::default(),
            providers: config::ProvidersResolved {
                warehouse: config::WarehouseResolved {
                    kind: react_core::resolved_config::WarehouseKind::Athena,
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: config::VectorResolved { enabled: false },
            },
        })
    }

    struct MockQueryProvider;
    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn query(&self, _sql: &str) -> Result<react_core::providers::QueryResult, String> {
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
            _dataset_id: &str,
            _name: &str,
            _sql: &str,
        ) -> Result<String, String> {
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
        async fn validate_project(
            &self,
            _scope: &RequestScope,
            _args: &react_core::providers::DbtValidateArgs,
        ) -> Result<react_core::providers::DbtValidateResult, String> {
            Ok(react_core::providers::DbtValidateResult::default())
        }
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>, query: Arc<dyn QueryProvider>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: Arc::new(NullModel::new()),
            storage,
            scope,
            keyspace,
            query: Some(query),
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: Some(Arc::new(NoopDbtProvider)),
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        }
    }

    #[test]
    fn extract_source_calls_finds_pairs() {
        let sql = "select 1 from {{ source('raw','events') }} join {{source(\"raw\",\"users\")}} u on 1=1";
        let got = extract_source_calls(sql);
        assert_eq!(
            got,
            vec![
                ("raw".to_string(), "events".to_string()),
                ("raw".to_string(), "users".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn build_validate_fail_facts_includes_missing_columns_and_relation_schema() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let query: Arc<dyn QueryProvider> = Arc::new(MockQueryProvider);
        let ctx = make_ctx(storage.clone(), query);

        // Seed manifest.json that maps ref('stg_dep') to a concrete relation.
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string()
            + "/";
        let manifest_key = format!("{}target/manifest.json", base);
        let manifest = serde_json::json!({
            "nodes": {
                "model.data_engineer.stg_dep": {
                    "resource_type": "model",
                    "name": "stg_dep",
                    "database": "AwsDataCatalog",
                    "schema": "test_silver",
                    "alias": "stg_dep",
                    "original_file_path": "models/staging/stg_dep.sql"
                }
            }
        });
        storage
            .put_bytes(
                &manifest_key,
                manifest.to_string().as_bytes(),
                "application/json",
            )
            .await
            .unwrap();

        // Seed failing model file.
        let failing_rel = "models/marts/fct_x.sql";
        let failing_key = format!("{}{}", base, failing_rel);
        storage
            .put_bytes(
                &failing_key,
                b"select 1 from {{ ref('stg_dep') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let validate_obs = serde_json::json!({
            "errors": ["Runtime Error in model fct_x (models/marts/fct_x.sql)\n  line 1:1: Column 'session_id_from_event' cannot be resolved"],
            "logs": {
                "run_or_build": {
                    "stdout": "Failure in model fct_x (models/marts/fct_x.sql)\nRuntime Error in model fct_x (models/marts/fct_x.sql)\n"
                }
            }
        });
        let contract = crate::data_engineer::controller_event::validate_contract_from_observation(
            validate_obs,
        )
        .expect("validate contract");
        let facts = build_validate_fail_facts(
            &ctx,
            "Amazon Athena (engine v3 / Trino SQL)".to_string(),
            &contract,
            FactsScope::ValidateFail,
            FactsLimits::for_scope(FactsScope::ValidateFail),
        )
        .await;

        assert!(facts
            .targets
            .missing_columns
            .iter()
            .any(|c| c == "session_id_from_event"));
        assert!(facts
            .relations
            .iter()
            .any(|r| r.fqn == "AwsDataCatalog.test_silver.stg_dep"));
        let cols = facts
            .relations
            .iter()
            .find(|r| r.fqn == "AwsDataCatalog.test_silver.stg_dep")
            .map(|r| {
                r.columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(cols.contains(&"session_id"));
    }
}
