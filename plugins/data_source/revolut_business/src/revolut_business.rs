use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::{info, warn};

use crate::privacy::PrivacyConfig;
use crate::raw_envelope::redact_and_envelope;
use crate::revolut_api::{
    lookback_from_iso, map_account, map_transaction, map_transaction_legs, now_iso, record_id,
    RevolutApiClient, DEFAULT_API_BASE, FIXTURE_ENV,
};
use crate::streams::{
    resolve_streams, RevolutStream, StreamProfile, NAMESPACE_ACCOUNT_SNAPSHOT,
    NAMESPACE_ACCOUNT_SOURCE_RAW, NAMESPACE_SYNC_RUN_DAILY, NAMESPACE_TRANSACTION_FACT,
    NAMESPACE_TRANSACTION_LEG_FACT, NAMESPACE_TRANSACTION_LEG_SOURCE_RAW,
    NAMESPACE_TRANSACTION_SOURCE_RAW,
};

const ISSUER_DOMAIN_ENV: &str = "REVOLUT_ISSUER_DOMAIN";

fn default_lookback_days() -> u32 {
    90
}

fn default_min_query_interval_ms() -> u64 {
    300
}

fn default_api_base() -> String {
    DEFAULT_API_BASE.to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceRevolutBusinessPluginConfig {
    pub client_id: String,
    pub start_date: String,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default)]
    pub stream_profile: StreamProfile,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    #[serde(default = "default_min_query_interval_ms")]
    pub min_query_interval_ms: u64,
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub private_key_pem: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub issuer_domain: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyConfig,
}

impl DataSourceRevolutBusinessPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        let fixture_mode = std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false);
        if !fixture_mode && self.client_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "client_id is required",
            ));
        }
        NaiveDate::parse_from_str(self.start_date.trim(), "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        if !fixture_mode {
            let has_access = self
                .access_token
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
            let has_refresh = self
                .refresh_token
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
            if !has_access && !has_refresh {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "access_token or refresh_token is required",
                ));
            }
        }
        Ok(())
    }

    fn issuer_domain(&self) -> String {
        self.issuer_domain
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| {
                std::env::var(ISSUER_DOMAIN_ENV)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_default()
    }
}

pub struct DataSourceRevolutBusinessPlugin {
    config: DataSourceRevolutBusinessPluginConfig,
    connection_id: String,
}

impl DataSourceRevolutBusinessPlugin {
    pub fn new(config: DataSourceRevolutBusinessPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self {
            connection_id: config.client_id.trim().to_string(),
            config,
        })
    }

    async fn api_client(&self) -> Result<RevolutApiClient, std::io::Error> {
        let http = RetryableHttpClient::new(RetryConfig::default());
        let api_base = self.config.api_base.trim().to_string();
        if let Some(token) = self
            .config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(RevolutApiClient::new(
                http,
                api_base,
                token.trim().to_string(),
                self.config.min_query_interval_ms,
            ));
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(RevolutApiClient::new(
                http,
                api_base,
                "fixture".into(),
                self.config.min_query_interval_ms,
            ));
        }
        let refresh_token = self.config.refresh_token.as_deref().unwrap_or("").trim();
        let private_key_pem = self.config.private_key_pem.as_deref().unwrap_or("").trim();
        let issuer_domain = self.config.issuer_domain();
        if refresh_token.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refresh_token is required when access_token is not set",
            ));
        }
        if private_key_pem.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "private_key_pem is required to refresh access tokens",
            ));
        }
        if issuer_domain.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "issuer_domain is required for JWT client assertion (config or REVOLUT_ISSUER_DOMAIN)",
            ));
        }
        RevolutApiClient::from_refresh_token(
            http,
            api_base,
            self.config.client_id.trim(),
            private_key_pem,
            &issuer_domain,
            refresh_token,
            self.config.min_query_interval_ms,
        )
        .await
    }

    fn selected_streams(&self) -> Vec<RevolutStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<RevolutStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn lookback_from(&self) -> Result<String, std::io::Error> {
        lookback_from_iso(self.config.start_date.trim(), self.config.lookback_days)
    }

    fn redact_rows(&self, rows: &mut [Value]) -> u64 {
        let mut total = 0u64;
        for row in rows.iter_mut() {
            total += self.config.privacy.redact_row(row).properties_redacted;
        }
        total
    }

    pub fn namespace_contract(namespace: &str, lookback_days: u32) -> SourceNamespaceContract {
        match namespace {
            NAMESPACE_ACCOUNT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("account_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Revolut account balance snapshot (no account names)".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_TRANSACTION_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("transaction_id")],
                cursor: Some(FieldPath::single("created_at")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Revolut transaction facts".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_TRANSACTION_LEG_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("leg_id")],
                cursor: Some(FieldPath::single("created_at")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Revolut transaction leg facts".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_ACCOUNT_SOURCE_RAW
            | NAMESPACE_TRANSACTION_SOURCE_RAW
            | NAMESPACE_TRANSACTION_LEG_SOURCE_RAW => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("source_record_id"),
                    FieldPath::single("ingest_run_date"),
                ],
                cursor: Some(FieldPath::single("ingest_run_date")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: None,
                description: format!("Revolut raw envelope: {namespace}"),
                semantics: Some(SourceSemantics::EntityState),
            },
            NAMESPACE_SYNC_RUN_DAILY => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Revolut connector sync health".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            other => panic!("unknown revolut namespace: {other}"),
        }
    }

    fn submit_namespace(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        mut rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        self.redact_rows(&mut rows);
        let payload = rows
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        let offset_key = OffsetKey::new(namespace, run_date.to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("revolut://{}/{}", self.connection_id, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn build_raw_rows(
        &self,
        ingest_run_date: &str,
        source_stream: &str,
        rows: &[Value],
        id_keys: &[&str],
    ) -> (Vec<Value>, u64) {
        let mut raw_rows = Vec::with_capacity(rows.len());
        let mut redacted = 0u64;
        for row in rows {
            let source_id = record_id(row, id_keys).unwrap_or_else(|| "unknown".to_string());
            let source_uri = format!(
                "revolut://{}/{}/{}",
                self.connection_id, source_stream, source_id
            );
            let (envelope, stats) = redact_and_envelope(
                &self.config.privacy,
                ingest_run_date,
                &self.connection_id,
                source_stream,
                &source_id,
                &source_uri,
                row,
            );
            redacted += stats.properties_redacted;
            raw_rows.push(envelope);
        }
        (raw_rows, redacted)
    }

    async fn sync_accounts(
        &self,
        client: &RevolutApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let rows = client.list_accounts().await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_account(run_date, run_date, &self.connection_id, row))
            .collect();
        let (raw_rows, redacted) =
            self.build_raw_rows(run_date, "accounts", &rows, &["id", "account_id"]);
        Ok((curated, raw_rows, redacted))
    }
}

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[async_trait]
impl DataSource for DataSourceRevolutBusinessPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        [
            NAMESPACE_ACCOUNT_SNAPSHOT,
            NAMESPACE_TRANSACTION_FACT,
            NAMESPACE_TRANSACTION_LEG_FACT,
            NAMESPACE_ACCOUNT_SOURCE_RAW,
            NAMESPACE_TRANSACTION_SOURCE_RAW,
            NAMESPACE_TRANSACTION_LEG_SOURCE_RAW,
            NAMESPACE_SYNC_RUN_DAILY,
        ]
        .iter()
        .map(|ns| Self::namespace_contract(ns, self.config.lookback_days))
        .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = Utc::now().format("%Y-%m-%d").to_string();
        let started = Instant::now();
        let client = self.api_client().await?;
        let streams = self.streams_for_run(discover);
        let mut synced: Vec<&str> = Vec::new();
        let mut properties_redacted: u64 = 0;
        let api_errors: u64 = 0;
        let mut sync_transactions = false;
        let mut sync_legs = false;

        for stream in &streams {
            if matches!(stream, RevolutStream::Transactions) {
                sync_transactions = true;
            }
            if matches!(stream, RevolutStream::TransactionLegs) {
                sync_legs = true;
            }
        }

        let tx_rows = if sync_transactions || sync_legs {
            let from_iso = if discover {
                run_date.clone()
            } else {
                self.lookback_from()?
            };
            let to_iso = if discover { None } else { Some(now_iso()) };
            Some(
                client
                    .list_transactions(&from_iso, to_iso.as_deref())
                    .await?,
            )
        } else {
            None
        };

        for stream in &streams {
            let result: Result<(), std::io::Error> = match stream {
                RevolutStream::Accounts => {
                    let (curated, raw, redacted) = self.sync_accounts(&client, &run_date).await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_ACCOUNT_SNAPSHOT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_ACCOUNT_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                RevolutStream::Transactions => {
                    let rows = tx_rows.as_deref().unwrap_or(&[]);
                    let curated: Vec<Value> = rows
                        .iter()
                        .map(|row| map_transaction(&run_date, &self.connection_id, row))
                        .collect();
                    let (raw, redacted) = self.build_raw_rows(
                        &run_date,
                        "transactions",
                        rows,
                        &["id", "transaction_id"],
                    );
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_TRANSACTION_FACT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_TRANSACTION_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                RevolutStream::TransactionLegs => {
                    let rows = tx_rows.as_deref().unwrap_or(&[]);
                    let curated: Vec<Value> = rows
                        .iter()
                        .flat_map(|row| map_transaction_legs(&run_date, &self.connection_id, row))
                        .collect();
                    let mut leg_source_rows = Vec::new();
                    for tx in rows {
                        if let Some(legs) = tx.get("legs").and_then(|v| v.as_array()) {
                            for (index, leg) in legs.iter().enumerate() {
                                let mut leg_row = leg.clone();
                                if let Value::Object(map) = &mut leg_row {
                                    map.entry("transaction_id".to_string()).or_insert_with(|| {
                                        record_id(tx, &["id", "transaction_id"])
                                            .map(Value::String)
                                            .unwrap_or(Value::Null)
                                    });
                                    if !map.contains_key("leg_id") && !map.contains_key("id") {
                                        map.insert(
                                            "leg_id".to_string(),
                                            Value::String(format!(
                                                "{}:{}",
                                                record_id(tx, &["id", "transaction_id"])
                                                    .unwrap_or_else(|| "unknown".to_string()),
                                                index
                                            )),
                                        );
                                    }
                                }
                                leg_source_rows.push(leg_row);
                            }
                        }
                    }
                    let (raw, redacted) = self.build_raw_rows(
                        &run_date,
                        "transaction_legs",
                        &leg_source_rows,
                        &["leg_id", "id"],
                    );
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_TRANSACTION_LEG_FACT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_TRANSACTION_LEG_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                RevolutStream::Health => Ok(()),
            };
            match result {
                Ok(()) => synced.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "Revolut stream failed");
                    return Err(err);
                }
            }
        }

        let sync_row = json!({
            "run_date": run_date,
            "connection_id": self.connection_id,
            "status": "ok",
            "streams_synced": synced.join(","),
            "properties_redacted": properties_redacted,
            "api_errors": api_errors,
            "elapsed_ms": started.elapsed().as_millis() as i64,
        });
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_SYNC_RUN_DAILY,
            &run_date,
            vec![sync_row],
        )?;

        info!(
            connection_id = %self.connection_id,
            streams = %synced.join(","),
            elapsed_ms = started.elapsed().as_millis(),
            "Revolut sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::streams::{
        NAMESPACE_ACCOUNT_SNAPSHOT, NAMESPACE_SYNC_RUN_DAILY, NAMESPACE_TRANSACTION_FACT,
        NAMESPACE_TRANSACTION_SOURCE_RAW,
    };
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn test_fixture_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_TEST_LOCK.lock().unwrap();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        set_fixture_dir(fixture_dir);
        guard
    }

    #[test]
    fn fact_namespaces_use_merge_by_key() {
        let _guard = test_fixture_guard();
        let plugin = DataSourceRevolutBusinessPlugin::new(sample_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        let tx = contracts
            .iter()
            .find(|c| c.namespace == NAMESPACE_TRANSACTION_FACT)
            .unwrap();
        assert_eq!(tx.write_policy, WritePolicy::MergeByKey);
        assert_eq!(tx.refresh_window, Some(90));
    }

    #[test]
    fn namespace_contracts_validate() {
        let _guard = test_fixture_guard();
        let plugin = DataSourceRevolutBusinessPlugin::new(sample_config()).unwrap();
        for contract in plugin.source_namespace_contracts() {
            contract.validate().expect("valid contract");
        }
    }

    #[test]
    fn sync_run_contract_uses_run_date_partition() {
        let _guard = test_fixture_guard();
        let contract =
            DataSourceRevolutBusinessPlugin::namespace_contract(NAMESPACE_SYNC_RUN_DAILY, 90);
        assert_eq!(contract.write_policy, WritePolicy::ReplacePartition);
        assert_eq!(contract.partition_key[0].dotted(), "run_date");
    }

    #[tokio::test]
    async fn happy_sync() {
        let _guard = test_fixture_guard();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let plugin = DataSourceRevolutBusinessPlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let mut plugin = plugin;
        plugin.sync(ctx.clone()).await.unwrap();

        let accounts = ctx.rows_for_namespace(NAMESPACE_ACCOUNT_SNAPSHOT);
        assert!(!accounts.is_empty());
        assert_eq!(
            accounts[0]["account_id"],
            "f52c6c84-26b9-4e95-bbcf-99ed6523fb51"
        );
        assert!(accounts[0].get("name").is_none());

        let txs = ctx.rows_for_namespace(NAMESPACE_TRANSACTION_FACT);
        assert!(!txs.is_empty());
        assert_eq!(
            txs[0]["transaction_id"],
            "c2b97a7a-1a2b-4c3d-8e9f-0a1b2c3d4e5f"
        );
        assert!(txs[0].get("reference").is_none());
        assert!(txs[0].get("merchant").is_none());

        let raw = ctx.rows_for_namespace(NAMESPACE_TRANSACTION_SOURCE_RAW);
        assert!(!raw.is_empty());
        assert!(raw[0].get("payload").is_none());
        assert!(raw[0].get("payload_sha256").is_some());

        let runs = ctx.rows_for_namespace(NAMESPACE_SYNC_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");

        clear_fixture_dir();
    }
}
