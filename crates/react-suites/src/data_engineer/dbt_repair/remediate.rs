use react_core::agent::AgentCtx;
use crate::config::ReactResolvedConfig;
use react_core::llm::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemediationChange {
    pub key: String,
    pub reason: Option<String>,
    pub changed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemediationReport {
    pub dialect: String,
    pub phase: String,
    pub scanned_files: usize,
    pub changed_files: usize,
    #[serde(default)]
    pub changes: Vec<RemediationChange>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct LlmRemediationResponse {
    #[serde(default)]
    changes: Vec<LlmChange>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LlmChange {
    key: String,
    patch_text: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LlmRemediationDecision {
    #[serde(default)]
    pub should_remediate: bool,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub reason: String,
}

pub fn active_provider_dialect(cfg: &ReactResolvedConfig) -> String {
    // Human-readable label, consumed by the LLM prompt. Keep it stable (used in logs/tool outputs).
    if cfg.providers.athena.enabled {
        // Athena Engine v3 uses Trino SQL.
        return "Amazon Athena (engine v3 / Trino SQL)".to_string();
    }
    "Unknown SQL dialect".to_string()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn parse_json_from_llm(text: &str) -> Result<Value, String> {
    // The prompt instructs JSON-only, but be resilient to accidental wrappers.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let s = text.trim();
    let start = s.find('{').ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    let end = s.rfind('}').ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    if end <= start {
        return Err("LLM response JSON object bounds invalid".to_string());
    }
    serde_json::from_str::<Value>(&s[start..=end]).map_err(|e| e.to_string())
}

pub fn llm_should_remediate_sql(
    ctx: &AgentCtx,
    dialect: &str,
    phase: &str,
    error_brief: &str,
) -> Result<LlmRemediationDecision, String> {
    // Strict JSON-only contract. This is a lightweight classifier to decide whether to run the
    // expensive, potentially invasive remediation pass.
    let sys = format!(
        "You are a SQL dialect expert.\n\
         Task: decide if dbt SQL files likely need *dialect/syntax* remediation.\n\
         Dialect: {dialect}\n\
         Phase: {phase}\n\
         Return ONLY valid JSON (no markdown, no commentary).\n\
         Output schema:\n\
         {{\"should_remediate\":true|false,\"confidence\":0.0-1.0,\"reason\":\"...\"}}\n\
         Rules:\n\
         - Only set should_remediate=true when you are confident errors are caused by dialect/syntax incompatibility.\n\
         - If errors are about missing nodes/models/sources, permissions, missing tables, or data issues, set should_remediate=false.\n"
    );
    let user = serde_json::json!({
        "error_brief": error_brief,
    })
    .to_string();

    let resp_text = ctx
        .llm
        .chat(&[
            ChatMessage { role: "system".to_string(), content: sys },
            ChatMessage { role: "user".to_string(), content: user },
        ])
        .map_err(|e| format!("llm should_remediate call failed: {}", e))?;
    let v = parse_json_from_llm(&resp_text)?;
    let mut parsed: LlmRemediationDecision =
        serde_json::from_value(v).map_err(|e| format!("failed to parse remediation decision JSON: {}", e))?;
    if !(0.0..=1.0).contains(&parsed.confidence) {
        // Be conservative on malformed values.
        parsed.confidence = parsed.confidence.max(0.0).min(1.0);
    }
    Ok(parsed)
}

pub async fn list_sql_keys_for_scope(ctx: &AgentCtx) -> Result<Vec<String>, String> {
    let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string() + "/";
    let keys = ctx.storage.list_prefix(&base).await.unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if !k.ends_with(".sql") {
            continue;
        }
        if k.contains("/target/") || k.contains("/_versions/") {
            continue;
        }
        out.push(k);
    }
    out.sort();
    Ok(out)
}

pub async fn remediate_dbt_sql_keys_with_llm(ctx: &AgentCtx, phase: &str, keys: &[String]) -> Result<RemediationReport, String> {
    let Some(cfg) = crate::config::resolved_config_from_ctx(ctx) else {
        return Ok(RemediationReport {
            dialect: "Unknown SQL dialect".to_string(),
            phase: phase.to_string(),
            scanned_files: 0,
            changed_files: 0,
            skipped: true,
            error: Some("resolved_config missing".to_string()),
            ..Default::default()
        });
    };
    let dialect = active_provider_dialect(cfg);

    let mut keys: Vec<String> = keys.iter().cloned().collect();
    keys.sort();
    keys.dedup();
    let scanned_files = keys.len();
    if scanned_files == 0 {
        return Ok(RemediationReport {
            dialect,
            phase: phase.to_string(),
            scanned_files,
            changed_files: 0,
            skipped: true,
            ..Default::default()
        });
    }

    tracing::info!(
        target: "dbt_sql_remediate",
        phase = %phase,
        dialect = %dialect,
        scanned_files = scanned_files,
        "starting"
    );

    // Chunking: keep each LLM call bounded.
    // We bias toward fewer files per call rather than truncating any file.
    let max_chars_per_batch: usize = 45_000;
    let mut idx: usize = 0;
    let mut report = RemediationReport {
        dialect: dialect.clone(),
        phase: phase.to_string(),
        scanned_files,
        ..Default::default()
    };

    while idx < keys.len() {
        let mut batch: Vec<(String, String)> = Vec::new(); // (key, content)
        let mut chars: usize = 0;
        while idx < keys.len() {
            let k = &keys[idx];
            let bytes = ctx.storage.get_bytes(k).await.unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            let add = k.len() + text.len();
            if !batch.is_empty() && chars + add > max_chars_per_batch {
                break;
            }
            chars += add;
            batch.push((k.clone(), text));
            idx += 1;
        }

        let input_files: Vec<Value> = batch
            .iter()
            .map(|(k, c)| serde_json::json!({ "key": k, "content": c }))
            .collect();
        let mut content_by_key: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (k, c) in batch.iter() {
            content_by_key.insert(k.clone(), c.clone());
        }

        // Strict JSON-only contract so we can apply changes deterministically.
        let sys = format!(
            "You are a meticulous SQL dialect remediation assistant.\n\
             Task: rewrite dbt SQL files so they are valid for the configured warehouse dialect.\n\
             Dialect: {dialect}\n\
             Constraints:\n\
             - Do not change business logic or semantics.\n\
             - Apply the smallest edit necessary to make SQL valid for the dialect.\n\
             - Do not invent new tables/columns.\n\
             - Output MUST be valid JSON only (no markdown, no commentary).\n\
             Output schema:\n\
             {{\"changes\":[{{\"key\":\"...\",\"patch_text\":\"...\",\"reason\":\"...\"}}],\"notes\":[\"...\"]}}\n\
             Only include a file in changes if you actually modify it.\n"
        );
        let user = serde_json::json!({
            "phase": phase,
            "files": input_files,
        })
        .to_string();

        let resp_text = ctx
            .llm
            .chat(&[
                ChatMessage { role: "system".to_string(), content: sys },
                ChatMessage { role: "user".to_string(), content: user },
            ])
            .map_err(|e| format!("dialect remediation LLM call failed: {}", e))?;

        let v = parse_json_from_llm(&resp_text)?;
        let parsed: LlmRemediationResponse =
            serde_json::from_value(v).map_err(|e| format!("failed to parse remediation JSON: {}", e))?;

        for n in parsed.notes.iter() {
            if !n.trim().is_empty() {
                report.notes.push(n.clone());
            }
        }

        for ch in parsed.changes.into_iter() {
            if ch.key.trim().is_empty() {
                continue;
            }
            // Only apply if key is within the scanned set (safety).
            if !keys.iter().any(|k| k == &ch.key) {
                continue;
            }
            let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
            let rel = ch
                .key
                .strip_prefix(&(base.clone() + "/"))
                .ok_or_else(|| format!("remediation key not under dbt prefix: {}", ch.key))?
                .to_string();
            let expected_base = content_by_key
                .get(&ch.key)
                .map(|s| sha256_hex(s))
                .unwrap_or_else(|| String::new());
            let outcome = crate::data_engineer::project_fs::apply_patch(
                ctx,
                None,
                &rel,
                &ch.patch_text,
                if expected_base.is_empty() { None } else { Some(expected_base.as_str()) },
                false,
            )
            .await?;
            ctx.storage
                .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/sql")
                .await?;
            report.changed_files += 1;
            report.changes.push(RemediationChange {
                key: ch.key,
                reason: ch.reason,
                changed: true,
            });
        }
    }

    // Best-effort: add a note for traceability.
    if report.changed_files > 0 {
        report.notes.push(format!("remediation_epoch_secs={}", now_epoch_secs()));
    }

    tracing::info!(
        target: "dbt_sql_remediate",
        phase = %phase,
        dialect = %dialect,
        changed_files = report.changed_files,
        "finished"
    );
    Ok(report)
}

pub async fn remediate_dbt_sql_with_llm(ctx: &AgentCtx, phase: &str) -> Result<RemediationReport, String> {
    let keys = list_sql_keys_for_scope(ctx).await?;
    remediate_dbt_sql_keys_with_llm(ctx, phase, &keys).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use react_core::llm::LargeLanguageModel;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockLlm {
        // Queue of responses to return from chat()
        chat_responses: Mutex<Vec<String>>,
        calls: Mutex<usize>,
    }

    impl LargeLanguageModel for MockLlm {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            let mut q = self.chat_responses.lock().unwrap();
            if q.is_empty() {
                return Err("no mock responses remaining".to_string());
            }
            Ok(q.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn minimal_cfg_athena() -> Arc<ReactResolvedConfig> {
        Arc::new(ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: "wg".to_string(),
                    region: "eu-west-1".to_string(),
                    result_s3: "s3://x/".to_string(),
                    target_catalog: "AwsDataCatalog".to_string(),
                    source_schema: "src".to_string(),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "src".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>, llm: Arc<dyn LargeLanguageModel>) -> AgentCtx {
        let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
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
            llm,
            storage,
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg_athena() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[tokio::test]
    async fn list_sql_keys_filters_target_and_versions() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm::default());
        let ctx = make_ctx(storage.clone(), llm);
        let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
        storage.put_bytes(&format!("{}/models/a.sql", base), b"select 1", "text/sql").await.unwrap();
        storage.put_bytes(&format!("{}/target/manifest.sql", base), b"no", "text/sql").await.unwrap();
        storage.put_bytes(&format!("{}/models/_versions/1.sql", base), b"no", "text/sql").await.unwrap();
        let keys = list_sql_keys_for_scope(&ctx).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].ends_with("/models/a.sql"));
    }

    #[tokio::test]
    async fn remediation_applies_llm_changes_to_storage() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let mock = MockLlm::default();
        let patch_text = crate::data_engineer::project_fs::create_patch_text("select 1", "select 2");
        *mock.chat_responses.lock().unwrap() = vec![serde_json::json!({
            "changes": [
                {"key":"t/w/p/dbt/models/m.sql","patch_text":patch_text,"reason":"minimal"}
            ],
            "notes": ["ok"]
        })
        .to_string()];
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(mock);
        let ctx = make_ctx(storage.clone(), llm);
        storage.put_bytes("t/w/p/dbt/models/m.sql", b"select 1", "text/sql").await.unwrap();
        let rep = remediate_dbt_sql_with_llm(&ctx, "pre_validate").await.unwrap();
        assert_eq!(rep.changed_files, 1);
        let bytes = storage.get_bytes("t/w/p/dbt/models/m.sql").await.unwrap();
        let got = String::from_utf8_lossy(&bytes);
        assert!(got.contains("select 2"));
        assert!(got.contains("config(schema=\"warehouse\""));
    }
}
