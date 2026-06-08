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

use crate::privacy::{PrivacyConfig, PrivacyStats};
use crate::raw_envelope::redact_and_envelope;
use crate::streams::{
    resolve_streams, StreamProfile, SumUpStream, NAMESPACE_CHECKOUT_SNAPSHOT,
    NAMESPACE_CHECKOUT_SOURCE_RAW, NAMESPACE_MERCHANT_SNAPSHOT, NAMESPACE_MERCHANT_SOURCE_RAW,
    NAMESPACE_PAYOUT_FACT, NAMESPACE_PAYOUT_SOURCE_RAW, NAMESPACE_SYNC_RUN_DAILY,
    NAMESPACE_TRANSACTION_FACT, NAMESPACE_TRANSACTION_SOURCE_RAW,
};
use crate::sumup_api::{
    map_checkout, map_merchant, map_payout, map_transaction, record_id, SumUpApiClient,
    DEFAULT_TOKEN_URL, FIXTURE_ENV,
};

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceSumUpPluginConfig {
    pub merchant_code: String,
    pub start_date: String,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default)]
    pub stream_profile: StreamProfile,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    #[serde(default = "default_min_query_interval_ms")]
    pub min_query_interval_ms: u64,
    #[serde(default = "default_oauth_token_url")]
    pub oauth_token_url: String,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyConfig,
}

fn default_lookback_days() -> u32 {
    7
}

fn default_min_query_interval_ms() -> u64 {
    200
}

fn default_oauth_token_url() -> String {
    DEFAULT_TOKEN_URL.to_string()
}

impl DataSourceSumUpPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.merchant_code.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "merchant_code is required",
            ));
        }
        NaiveDate::parse_from_str(self.start_date.trim(), "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        Ok(())
    }
}

pub struct DataSourceSumUpPlugin {
    config: DataSourceSumUpPluginConfig,
    merchant_code: String,
}

impl DataSourceSumUpPlugin {
    pub fn new(config: DataSourceSumUpPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self {
            merchant_code: config.merchant_code.trim().to_string(),
            config,
        })
    }

    async fn api_client(&self) -> Result<SumUpApiClient, std::io::Error> {
        let http = RetryableHttpClient::new(RetryConfig::default());
        if let Some(token) = self
            .config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(SumUpApiClient::new(
                http,
                self.merchant_code.clone(),
                token.trim().to_string(),
                self.config.min_query_interval_ms,
            ));
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(SumUpApiClient::new(
                http,
                self.merchant_code.clone(),
                "fixture".into(),
                self.config.min_query_interval_ms,
            ));
        }
        SumUpApiClient::from_oauth(
            http,
            self.merchant_code.clone(),
            self.config.oauth_token_url.trim(),
            self.config.oauth_client_id.as_deref().unwrap_or(""),
            self.config.oauth_client_secret.as_deref().unwrap_or(""),
            self.config.oauth_refresh_token.as_deref().unwrap_or(""),
            self.config.min_query_interval_ms,
        )
        .await
    }

    fn selected_streams(&self) -> Vec<SumUpStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<SumUpStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn lookback_start_date(&self) -> Result<String, std::io::Error> {
        let start = NaiveDate::parse_from_str(self.config.start_date.trim(), "%Y-%m-%d")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let effective = start - chrono::Duration::days(self.config.lookback_days as i64);
        Ok(effective.format("%Y-%m-%d").to_string())
    }

    fn lookback_end_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn oldest_time_iso(&self) -> Result<String, std::io::Error> {
        let start = NaiveDate::parse_from_str(self.config.start_date.trim(), "%Y-%m-%d")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let effective = start - chrono::Duration::days(self.config.lookback_days as i64);
        Ok(effective
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string())
    }

    fn redact(&self, row: &mut Value) -> PrivacyStats {
        self.config.privacy.redact_row(row)
    }

    fn redact_rows(&self, rows: &mut [Value]) -> u64 {
        let mut total = 0u64;
        for row in rows.iter_mut() {
            total += self.redact(row).properties_redacted;
        }
        total
    }

    pub fn namespace_contract(namespace: &str, lookback_days: u32) -> SourceNamespaceContract {
        match namespace {
            NAMESPACE_MERCHANT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("merchant_code"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "SumUp merchant profile snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_TRANSACTION_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("transaction_id")],
                cursor: Some(FieldPath::single("timestamp")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "SumUp payment transactions".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_PAYOUT_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("payout_id")],
                cursor: Some(FieldPath::single("date")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "SumUp merchant payouts".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_CHECKOUT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("checkout_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "SumUp checkout snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_MERCHANT_SOURCE_RAW
            | NAMESPACE_TRANSACTION_SOURCE_RAW
            | NAMESPACE_PAYOUT_SOURCE_RAW
            | NAMESPACE_CHECKOUT_SOURCE_RAW => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("source_record_id"),
                    FieldPath::single("ingest_run_date"),
                ],
                cursor: Some(FieldPath::single("ingest_run_date")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: None,
                description: format!("SumUp raw envelope: {namespace}"),
                semantics: Some(SourceSemantics::EntityState),
            },
            NAMESPACE_SYNC_RUN_DAILY => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "SumUp connector sync health".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            other => panic!("unknown sumup namespace: {other}"),
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
                source_uri: format!("sumup://{}/{}", self.merchant_code, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    async fn sync_merchant(
        &self,
        client: &SumUpApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let body = client.merchant().await?;
        let curated = vec![map_merchant(run_date, run_date, &self.merchant_code, &body)];
        let source_id = self.merchant_code.clone();
        let (raw, stats) = redact_and_envelope(
            &self.config.privacy,
            run_date,
            &self.merchant_code,
            "merchant",
            &source_id,
            &format!("sumup://{}/merchant/{}", self.merchant_code, source_id),
            &body,
        );
        Ok((curated, vec![raw], stats.properties_redacted))
    }

    async fn sync_transactions(
        &self,
        client: &SumUpApiClient,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let oldest_time = if discover {
            None
        } else {
            Some(self.oldest_time_iso()?)
        };
        let rows = client
            .list_transactions(oldest_time.as_deref(), None)
            .await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_transaction(run_date, &self.merchant_code, row))
            .collect();
        let mut raw_rows = Vec::with_capacity(rows.len());
        let mut redacted = 0u64;
        for row in &rows {
            let source_id =
                record_id(row, &["transaction_id", "id"]).unwrap_or_else(|| "unknown".to_string());
            let (envelope, stats) = redact_and_envelope(
                &self.config.privacy,
                run_date,
                &self.merchant_code,
                "transactions",
                &source_id,
                &format!("sumup://{}/transactions/{}", self.merchant_code, source_id),
                row,
            );
            redacted += stats.properties_redacted;
            raw_rows.push(envelope);
        }
        Ok((curated, raw_rows, redacted))
    }

    async fn sync_payouts(
        &self,
        client: &SumUpApiClient,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let (start_date, end_date) = if discover {
            (run_date.to_string(), run_date.to_string())
        } else {
            (self.lookback_start_date()?, self.lookback_end_date())
        };
        let rows = client.list_payouts(&start_date, &end_date).await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_payout(run_date, &self.merchant_code, row))
            .collect();
        let mut raw_rows = Vec::with_capacity(rows.len());
        let mut redacted = 0u64;
        for row in &rows {
            let source_id =
                record_id(row, &["id", "payout_id"]).unwrap_or_else(|| "unknown".to_string());
            let (envelope, stats) = redact_and_envelope(
                &self.config.privacy,
                run_date,
                &self.merchant_code,
                "payouts",
                &source_id,
                &format!("sumup://{}/payouts/{}", self.merchant_code, source_id),
                row,
            );
            redacted += stats.properties_redacted;
            raw_rows.push(envelope);
        }
        Ok((curated, raw_rows, redacted))
    }

    async fn sync_checkouts(
        &self,
        client: &SumUpApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let rows = client.list_checkouts().await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_checkout(run_date, run_date, &self.merchant_code, row))
            .collect();
        let mut raw_rows = Vec::with_capacity(rows.len());
        let mut redacted = 0u64;
        for row in &rows {
            let source_id =
                record_id(row, &["id", "checkout_id"]).unwrap_or_else(|| "unknown".to_string());
            let (envelope, stats) = redact_and_envelope(
                &self.config.privacy,
                run_date,
                &self.merchant_code,
                "checkouts",
                &source_id,
                &format!("sumup://{}/checkouts/{}", self.merchant_code, source_id),
                row,
            );
            redacted += stats.properties_redacted;
            raw_rows.push(envelope);
        }
        Ok((curated, raw_rows, redacted))
    }
}

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[async_trait]
impl DataSource for DataSourceSumUpPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        [
            NAMESPACE_MERCHANT_SNAPSHOT,
            NAMESPACE_TRANSACTION_FACT,
            NAMESPACE_PAYOUT_FACT,
            NAMESPACE_CHECKOUT_SNAPSHOT,
            NAMESPACE_MERCHANT_SOURCE_RAW,
            NAMESPACE_TRANSACTION_SOURCE_RAW,
            NAMESPACE_PAYOUT_SOURCE_RAW,
            NAMESPACE_CHECKOUT_SOURCE_RAW,
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

        for stream in &streams {
            let result: Result<(), std::io::Error> = match stream {
                SumUpStream::Merchant => {
                    let (curated, raw, redacted) = self.sync_merchant(&client, &run_date).await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_MERCHANT_SNAPSHOT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_MERCHANT_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                SumUpStream::Transactions => {
                    let (curated, raw, redacted) =
                        self.sync_transactions(&client, &run_date, discover).await?;
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
                SumUpStream::Payouts => {
                    let (curated, raw, redacted) =
                        self.sync_payouts(&client, &run_date, discover).await?;
                    properties_redacted += redacted;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_PAYOUT_FACT, &run_date, curated)?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_PAYOUT_SOURCE_RAW, &run_date, raw)
                }
                SumUpStream::Checkouts => {
                    let (curated, raw, redacted) = self.sync_checkouts(&client, &run_date).await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CHECKOUT_SNAPSHOT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CHECKOUT_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                SumUpStream::Health => Ok(()),
            };
            match result {
                Ok(()) => synced.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "SumUp stream failed");
                    return Err(err);
                }
            }
        }

        let sync_row = json!({
            "run_date": run_date,
            "merchant_code": self.merchant_code,
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
            merchant_code = %self.merchant_code,
            streams = %synced.join(","),
            elapsed_ms = started.elapsed().as_millis(),
            "SumUp sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::streams::{NAMESPACE_SYNC_RUN_DAILY, NAMESPACE_TRANSACTION_FACT};
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn fact_namespaces_use_merge_by_key() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let plugin = DataSourceSumUpPlugin::new(sample_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        let tx = contracts
            .iter()
            .find(|c| c.namespace == NAMESPACE_TRANSACTION_FACT)
            .unwrap();
        assert_eq!(tx.write_policy, WritePolicy::MergeByKey);
        assert_eq!(tx.refresh_window, Some(30));
        let payout = contracts
            .iter()
            .find(|c| c.namespace == NAMESPACE_PAYOUT_FACT)
            .unwrap();
        assert_eq!(payout.write_policy, WritePolicy::MergeByKey);
    }

    #[test]
    fn namespace_contracts_validate() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let plugin = DataSourceSumUpPlugin::new(sample_config()).unwrap();
        for contract in plugin.source_namespace_contracts() {
            contract.validate().expect("valid contract");
        }
    }

    #[test]
    fn sync_run_contract_uses_run_date_partition() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let contract = DataSourceSumUpPlugin::namespace_contract(NAMESPACE_SYNC_RUN_DAILY, 7);
        assert_eq!(contract.write_policy, WritePolicy::ReplacePartition);
        assert_eq!(contract.partition_key[0].dotted(), "run_date");
    }

    #[tokio::test]
    async fn happy_sync() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        set_fixture_dir(fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let plugin = DataSourceSumUpPlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let mut plugin = plugin;
        plugin.sync(ctx.clone()).await.unwrap();

        let txs = ctx.rows_for_namespace(NAMESPACE_TRANSACTION_FACT);
        assert!(!txs.is_empty());
        assert_eq!(
            txs[0]["transaction_id"],
            "410fc44a-5956-44e1-b5cc-19c6f8d727a4"
        );
        assert!(txs[0].get("user").is_none());

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
