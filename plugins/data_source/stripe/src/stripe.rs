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
use crate::streams::{
    resolve_streams, StreamProfile, StripeStream, NAMESPACE_ACCOUNT_SNAPSHOT,
    NAMESPACE_BALANCE_TRANSACTION_FACT, NAMESPACE_CHARGE_FACT, NAMESPACE_COUPON_SNAPSHOT,
    NAMESPACE_CUSTOMER_SNAPSHOT, NAMESPACE_DISPUTE_FACT, NAMESPACE_INVOICE_FACT,
    NAMESPACE_INVOICE_LINE_FACT, NAMESPACE_PAYMENT_INTENT_FACT, NAMESPACE_PAYOUT_FACT,
    NAMESPACE_PRICE_SNAPSHOT, NAMESPACE_PRODUCT_SNAPSHOT, NAMESPACE_PROMOTION_CODE_SNAPSHOT,
    NAMESPACE_REFUND_FACT, NAMESPACE_SUBSCRIPTION_SNAPSHOT, NAMESPACE_SYNC_RUN_DAILY,
};
use crate::stripe_api::{
    map_account, map_balance_transaction, map_charge, map_coupon, map_customer, map_dispute,
    map_invoice, map_invoice_lines, map_payment_intent, map_payout, map_price, map_product,
    map_promotion_code, map_refund, map_subscription, StripeApiClient, FIXTURE_ENV,
};

/// Test-only override for Iceberg equality-commit e2e (`merge_by_key`, `replace_partition`).
pub const WRITE_POLICY_ENV: &str = "SKIPPR_STRIPE_WRITE_POLICY";

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceStripePluginConfig {
    pub stripe_account_id: String,
    pub start_date: String,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default)]
    pub stream_profile: StreamProfile,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    #[serde(default = "default_min_query_interval_ms")]
    pub min_query_interval_ms: u64,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub privacy: PrivacyConfig,
}

fn default_lookback_days() -> u32 {
    7
}

fn default_min_query_interval_ms() -> u64 {
    100
}

impl DataSourceStripePluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.stripe_account_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "stripe_account_id is required",
            ));
        }
        Ok(())
    }
}

pub struct DataSourceStripePlugin {
    config: DataSourceStripePluginConfig,
    stripe_account_id: String,
}

impl DataSourceStripePlugin {
    pub fn new(config: DataSourceStripePluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self {
            stripe_account_id: config.stripe_account_id.trim().to_string(),
            config,
        })
    }

    async fn api_client(&self) -> Result<StripeApiClient, std::io::Error> {
        let http = RetryableHttpClient::new(RetryConfig::default());
        if let Some(token) = self.config.access_token.as_deref().filter(|t| !t.trim().is_empty()) {
            return Ok(StripeApiClient::new(
                http,
                self.stripe_account_id.clone(),
                token.trim().to_string(),
                self.config.min_query_interval_ms,
            ));
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(StripeApiClient::new(
                http,
                self.stripe_account_id.clone(),
                "fixture".into(),
                self.config.min_query_interval_ms,
            ));
        }
        let token_url = self.config.oauth_token_url.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Stripe requires oauth_refresh_token + client credentials or SKIPPR_STRIPE_FIXTURE_DIR",
            )
        })?;
        StripeApiClient::from_oauth(
            http,
            self.stripe_account_id.clone(),
            token_url,
            self.config.oauth_client_id.as_deref().unwrap_or(""),
            self.config.oauth_client_secret.as_deref().unwrap_or(""),
            self.config.oauth_refresh_token.as_deref().unwrap_or(""),
            self.config.min_query_interval_ms,
        )
        .await
    }

    fn selected_streams(&self) -> Vec<StripeStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<StripeStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn created_gte(&self) -> Option<i64> {
        let start = NaiveDate::parse_from_str(self.config.start_date.trim(), "%Y-%m-%d")
            .ok()?
            .and_hms_opt(0, 0, 0)?;
        let utc = start.and_utc();
        let lookback = chrono::Duration::days(self.config.lookback_days as i64);
        let effective = utc - lookback;
        Some(effective.timestamp())
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

    fn write_policy_from_env(default: WritePolicy) -> WritePolicy {
        match std::env::var(WRITE_POLICY_ENV)
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("merge_by_key") => WritePolicy::MergeByKey,
            Some("replace_partition") => WritePolicy::ReplacePartition,
            _ => default,
        }
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let pk_id = match namespace {
            NAMESPACE_ACCOUNT_SNAPSHOT => "account_id",
            NAMESPACE_PRODUCT_SNAPSHOT => "product_id",
            NAMESPACE_PRICE_SNAPSHOT => "price_id",
            NAMESPACE_CUSTOMER_SNAPSHOT => "customer_id",
            NAMESPACE_SUBSCRIPTION_SNAPSHOT => "subscription_id",
            NAMESPACE_INVOICE_FACT => "invoice_id",
            NAMESPACE_INVOICE_LINE_FACT => "line_id",
            NAMESPACE_CHARGE_FACT => "charge_id",
            NAMESPACE_PAYMENT_INTENT_FACT => "payment_intent_id",
            NAMESPACE_REFUND_FACT => "refund_id",
            NAMESPACE_DISPUTE_FACT => "dispute_id",
            NAMESPACE_BALANCE_TRANSACTION_FACT => "balance_transaction_id",
            NAMESPACE_PAYOUT_FACT => "payout_id",
            NAMESPACE_COUPON_SNAPSHOT => "coupon_id",
            NAMESPACE_PROMOTION_CODE_SNAPSHOT => "promotion_code_id",
            NAMESPACE_SYNC_RUN_DAILY => "run_date",
            _ => "id",
        };
        SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single(pk_id),
                FieldPath::single("ingest_run_date"),
            ],
            cursor: Some(FieldPath::single("ingest_run_date")),
            partition_key: vec![FieldPath::single("ingest_run_date")],
            write_policy: Self::write_policy_from_env(WritePolicy::ReplacePartition),
            refresh_window: None,
            description: format!("Stripe {namespace}"),
            semantics: Some(SourceSemantics::MutableReport),
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
                source_uri: format!("stripe://{}/{}", self.stripe_account_id, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    async fn sync_account(
        &self,
        client: &StripeApiClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let body = client.account().await?;
        Ok(vec![map_account(
            run_date,
            &self.stripe_account_id,
            &body,
        )])
    }

    async fn sync_catalog(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let products = client
            .list_products(created_gte)
            .await?
            .iter()
            .map(|p| map_product(run_date, &self.stripe_account_id, p))
            .collect();
        let prices = client
            .list_prices(created_gte)
            .await?
            .iter()
            .map(|p| map_price(run_date, &self.stripe_account_id, p))
            .collect();
        Ok((products, prices))
    }

    async fn sync_customers(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        Ok(client
            .list_customers(created_gte)
            .await?
            .iter()
            .map(|c| map_customer(run_date, &self.stripe_account_id, c))
            .collect())
    }

    async fn sync_subscriptions(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        Ok(client
            .list_subscriptions(created_gte)
            .await?
            .iter()
            .map(|s| map_subscription(run_date, &self.stripe_account_id, s))
            .collect())
    }

    async fn sync_invoices(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let invoices = client.list_invoices(created_gte).await?;
        let mut facts = Vec::new();
        let mut lines = Vec::new();
        for inv in &invoices {
            facts.push(map_invoice(run_date, &self.stripe_account_id, inv));
            lines.extend(map_invoice_lines(run_date, &self.stripe_account_id, inv));
        }
        Ok((facts, lines))
    }

    async fn sync_payments(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>), std::io::Error> {
        let charges = client
            .list_charges(created_gte)
            .await?
            .iter()
            .map(|c| map_charge(run_date, &self.stripe_account_id, c))
            .collect();
        let intents = client
            .list_payment_intents(created_gte)
            .await?
            .iter()
            .map(|p| map_payment_intent(run_date, &self.stripe_account_id, p))
            .collect();
        let refunds = client
            .list_refunds(created_gte)
            .await?
            .iter()
            .map(|r| map_refund(run_date, &self.stripe_account_id, r))
            .collect();
        Ok((charges, intents, refunds))
    }

    async fn sync_disputes(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        Ok(client
            .list_disputes(created_gte)
            .await?
            .iter()
            .map(|d| map_dispute(run_date, &self.stripe_account_id, d))
            .collect())
    }

    async fn sync_cash(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let balance = client
            .list_balance_transactions(created_gte)
            .await?
            .iter()
            .map(|b| map_balance_transaction(run_date, &self.stripe_account_id, b))
            .collect();
        let payouts = client
            .list_payouts(created_gte)
            .await?
            .iter()
            .map(|p| map_payout(run_date, &self.stripe_account_id, p))
            .collect();
        Ok((balance, payouts))
    }

    async fn sync_promotions(
        &self,
        client: &StripeApiClient,
        run_date: &str,
        created_gte: Option<i64>,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let coupons = client
            .list_coupons(created_gte)
            .await?
            .iter()
            .map(|c| map_coupon(run_date, &self.stripe_account_id, c))
            .collect();
        let codes = client
            .list_promotion_codes(created_gte)
            .await?
            .iter()
            .map(|c| map_promotion_code(run_date, &self.stripe_account_id, c))
            .collect();
        Ok((coupons, codes))
    }
}

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[async_trait]
impl DataSource for DataSourceStripePlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        [
            NAMESPACE_ACCOUNT_SNAPSHOT,
            NAMESPACE_PRODUCT_SNAPSHOT,
            NAMESPACE_PRICE_SNAPSHOT,
            NAMESPACE_CUSTOMER_SNAPSHOT,
            NAMESPACE_SUBSCRIPTION_SNAPSHOT,
            NAMESPACE_INVOICE_FACT,
            NAMESPACE_INVOICE_LINE_FACT,
            NAMESPACE_CHARGE_FACT,
            NAMESPACE_PAYMENT_INTENT_FACT,
            NAMESPACE_REFUND_FACT,
            NAMESPACE_DISPUTE_FACT,
            NAMESPACE_BALANCE_TRANSACTION_FACT,
            NAMESPACE_PAYOUT_FACT,
            NAMESPACE_COUPON_SNAPSHOT,
            NAMESPACE_PROMOTION_CODE_SNAPSHOT,
            NAMESPACE_SYNC_RUN_DAILY,
        ]
        .iter()
        .map(|ns| Self::namespace_contract(ns))
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
        let created_gte = if discover { None } else { self.created_gte() };
        let mut synced: Vec<&str> = Vec::new();
        let properties_redacted: u64 = 0;
        let api_errors: u64 = 0;

        for stream in &streams {
            let result: Result<(), std::io::Error> = match stream {
                StripeStream::Account => {
                    let rows = self.sync_account(&client, &run_date).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_ACCOUNT_SNAPSHOT, &run_date, rows)
                }
                StripeStream::Catalog => {
                    let (products, prices) =
                        self.sync_catalog(&client, &run_date, created_gte).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PRODUCT_SNAPSHOT,
                        &run_date,
                        products,
                    )?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_PRICE_SNAPSHOT, &run_date, prices)
                }
                StripeStream::Customers => {
                    let rows = self.sync_customers(&client, &run_date, created_gte).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CUSTOMER_SNAPSHOT,
                        &run_date,
                        rows,
                    )
                }
                StripeStream::Subscriptions => {
                    let rows = self
                        .sync_subscriptions(&client, &run_date, created_gte)
                        .await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_SUBSCRIPTION_SNAPSHOT,
                        &run_date,
                        rows,
                    )
                }
                StripeStream::Invoices => {
                    let (facts, lines) =
                        self.sync_invoices(&client, &run_date, created_gte).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_INVOICE_FACT, &run_date, facts)?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_INVOICE_LINE_FACT,
                        &run_date,
                        lines,
                    )
                }
                StripeStream::Payments => {
                    let (charges, intents, refunds) =
                        self.sync_payments(&client, &run_date, created_gte).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_CHARGE_FACT, &run_date, charges)?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PAYMENT_INTENT_FACT,
                        &run_date,
                        intents,
                    )?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_REFUND_FACT, &run_date, refunds)
                }
                StripeStream::Disputes => {
                    let rows = self.sync_disputes(&client, &run_date, created_gte).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_DISPUTE_FACT, &run_date, rows)
                }
                StripeStream::Cash => {
                    let (balance, payouts) =
                        self.sync_cash(&client, &run_date, created_gte).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_BALANCE_TRANSACTION_FACT,
                        &run_date,
                        balance,
                    )?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_PAYOUT_FACT, &run_date, payouts)
                }
                StripeStream::Promotions => {
                    let (coupons, codes) =
                        self.sync_promotions(&client, &run_date, created_gte).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_COUPON_SNAPSHOT, &run_date, coupons)?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PROMOTION_CODE_SNAPSHOT,
                        &run_date,
                        codes,
                    )
                }
            };
            match result {
                Ok(()) => synced.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "Stripe stream failed");
                    return Err(err);
                }
            }
        }

        let sync_row = json!({
            "run_date": run_date,
            "stripe_account_id": self.stripe_account_id,
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
            stripe_account_id = %self.stripe_account_id,
            streams = %synced.join(","),
            elapsed_ms = started.elapsed().as_millis(),
            "Stripe sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::streams::{
        NAMESPACE_CHARGE_FACT, NAMESPACE_CUSTOMER_SNAPSHOT, NAMESPACE_SUBSCRIPTION_SNAPSHOT,
        NAMESPACE_SYNC_RUN_DAILY,
    };
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn write_policy_env_override_applies_merge_by_key() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var(WRITE_POLICY_ENV, "merge_by_key");
        let contract = DataSourceStripePlugin::namespace_contract(NAMESPACE_CHARGE_FACT);
        assert_eq!(contract.write_policy, WritePolicy::MergeByKey);
        std::env::remove_var(WRITE_POLICY_ENV);
    }

    #[test]
    fn namespace_contracts_validate() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        std::env::remove_var(WRITE_POLICY_ENV);
        let mut cfg = sample_config();
        cfg.access_token = Some("sk_test".into());
        let plugin = DataSourceStripePlugin::new(cfg).unwrap();
        for contract in plugin.source_namespace_contracts() {
            contract.validate().expect("valid contract");
        }
    }

    #[tokio::test]
    async fn happy_sync() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        set_fixture_dir(fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let plugin = DataSourceStripePlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let mut plugin = plugin;
        plugin.sync(ctx.clone()).await.unwrap();

        let subs = ctx.rows_for_namespace(NAMESPACE_SUBSCRIPTION_SNAPSHOT);
        assert!(!subs.is_empty());
        assert_eq!(subs[0]["subscription_id"], "sub_fixture1");
        assert!(subs[0].get("email").is_none());

        let charges = ctx.rows_for_namespace(NAMESPACE_CHARGE_FACT);
        assert!(!charges.is_empty());
        assert_eq!(charges[0]["charge_id"], "ch_fixture1");

        let customers = ctx.rows_for_namespace(NAMESPACE_CUSTOMER_SNAPSHOT);
        assert!(!customers.is_empty());
        assert!(customers[0].get("email").is_none());

        let runs = ctx.rows_for_namespace(NAMESPACE_SYNC_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");

        clear_fixture_dir();
    }
}
