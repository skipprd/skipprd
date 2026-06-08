use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, submit_payload_batches, IngestBatch,
};
use tracing::{info, warn};

use crate::privacy::{PrivacyConfig, PrivacyStats};
use crate::raw_envelope::redact_and_envelope;
use crate::streams::{
    resolve_streams, StreamProfile, XeroStream, NAMESPACE_ACCOUNT_SNAPSHOT,
    NAMESPACE_ACCOUNT_SOURCE_RAW, NAMESPACE_BANK_TRANSACTION_FACT,
    NAMESPACE_BANK_TRANSACTION_SOURCE_RAW, NAMESPACE_CONTACT_SNAPSHOT,
    NAMESPACE_CONTACT_SOURCE_RAW, NAMESPACE_INVOICE_FACT, NAMESPACE_INVOICE_LINE_FACT,
    NAMESPACE_INVOICE_LINE_SOURCE_RAW, NAMESPACE_INVOICE_SOURCE_RAW,
    NAMESPACE_ORGANISATION_SNAPSHOT, NAMESPACE_ORGANISATION_SOURCE_RAW, NAMESPACE_PAYMENT_FACT,
    NAMESPACE_PAYMENT_SOURCE_RAW, NAMESPACE_SYNC_RUN_DAILY,
};
use crate::xero_api::{
    map_account, map_bank_transaction, map_contact, map_invoice, map_invoice_lines,
    map_organisation, map_payment, max_updated_date_utc, record_id, XeroApiClient,
    DEFAULT_TOKEN_URL, FIXTURE_ENV,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XeroStreamCheckpoint {
    last_modified: String,
}

fn default_lookback_days() -> u32 {
    90
}

fn default_page_size() -> u32 {
    100
}

fn default_min_query_interval_ms() -> u64 {
    350
}

fn default_oauth_token_url() -> String {
    DEFAULT_TOKEN_URL.to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceXeroAccountingPluginConfig {
    pub tenant_id: String,
    pub start_date: String,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
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

impl DataSourceXeroAccountingPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.tenant_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tenant_id is required",
            ));
        }
        NaiveDate::parse_from_str(self.start_date.trim(), "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        if self.page_size == 0 || self.page_size > 100 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page_size must be between 1 and 100",
            ));
        }
        Ok(())
    }
}

pub struct DataSourceXeroAccountingPlugin {
    config: DataSourceXeroAccountingPluginConfig,
    tenant_id: String,
}

impl DataSourceXeroAccountingPlugin {
    pub fn new(config: DataSourceXeroAccountingPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self {
            tenant_id: config.tenant_id.trim().to_string(),
            config,
        })
    }

    async fn api_client(&self) -> Result<XeroApiClient, std::io::Error> {
        let http = RetryableHttpClient::new(RetryConfig::default());
        if let Some(token) = self
            .config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(XeroApiClient::new(
                http,
                self.tenant_id.clone(),
                token.trim().to_string(),
                self.config.min_query_interval_ms,
                self.config.page_size,
            ));
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(XeroApiClient::new(
                http,
                self.tenant_id.clone(),
                "fixture".into(),
                self.config.min_query_interval_ms,
                self.config.page_size,
            ));
        }
        XeroApiClient::from_oauth(
            http,
            self.tenant_id.clone(),
            self.config.oauth_token_url.trim(),
            self.config.oauth_client_id.as_deref().unwrap_or(""),
            self.config.oauth_client_secret.as_deref().unwrap_or(""),
            self.config.oauth_refresh_token.as_deref().unwrap_or(""),
            self.config.min_query_interval_ms,
            self.config.page_size,
        )
        .await
    }

    fn selected_streams(&self) -> Vec<XeroStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<XeroStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn lookback_where_clause(&self) -> Result<String, std::io::Error> {
        let start = NaiveDate::parse_from_str(self.config.start_date.trim(), "%Y-%m-%d")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let effective = start - chrono::Duration::days(self.config.lookback_days as i64);
        Ok(format!(
            "UpdatedDateUTC>=DateTime({},{},{})",
            effective.year(),
            effective.month(),
            effective.day()
        ))
    }

    fn checkpoint_key(&self, stream: &str) -> String {
        format!("xero:{}:{}", self.tenant_id, stream)
    }

    fn load_last_modified(ctx: &dyn SourceSyncContext, key: &str) -> Option<String> {
        load_checkpoint_payload::<XeroStreamCheckpoint>(ctx, key).map(|cp| cp.last_modified)
    }

    fn store_last_modified(
        ctx: &dyn SourceSyncContext,
        key: &str,
        last_modified: &str,
    ) -> Result<(), std::io::Error> {
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &XeroStreamCheckpoint {
                last_modified: last_modified.to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
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
            NAMESPACE_ORGANISATION_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("organisation_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Xero organisation snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
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
                description: "Xero chart of accounts snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_CONTACT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("contact_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Xero contact ID-only snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_INVOICE_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("invoice_id")],
                cursor: Some(FieldPath::single("updated_date_utc")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Xero invoices".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_INVOICE_LINE_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("invoice_id"),
                    FieldPath::single("line_id"),
                ],
                cursor: Some(FieldPath::single("ingest_run_date")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Xero invoice line items".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_PAYMENT_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("payment_id")],
                cursor: Some(FieldPath::single("updated_date_utc")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Xero payments".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_BANK_TRANSACTION_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("bank_transaction_id")],
                cursor: Some(FieldPath::single("updated_date_utc")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: Some(lookback_days),
                description: "Xero bank transactions".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_ORGANISATION_SOURCE_RAW
            | NAMESPACE_ACCOUNT_SOURCE_RAW
            | NAMESPACE_CONTACT_SOURCE_RAW
            | NAMESPACE_INVOICE_SOURCE_RAW
            | NAMESPACE_INVOICE_LINE_SOURCE_RAW
            | NAMESPACE_PAYMENT_SOURCE_RAW
            | NAMESPACE_BANK_TRANSACTION_SOURCE_RAW => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("source_record_id"),
                    FieldPath::single("ingest_run_date"),
                ],
                cursor: Some(FieldPath::single("ingest_run_date")),
                partition_key: vec![FieldPath::single("ingest_run_date")],
                write_policy: WritePolicy::MergeByKey,
                refresh_window: None,
                description: format!("Xero raw envelope: {namespace}"),
                semantics: Some(SourceSemantics::EntityState),
            },
            NAMESPACE_SYNC_RUN_DAILY => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Xero connector sync health".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            other => panic!("unknown xero namespace: {other}"),
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
                source_uri: format!("xero://{}/{}", self.tenant_id, namespace),
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
            let source_uri = format!("xero://{}/{}/{}", self.tenant_id, source_stream, source_id);
            let (envelope, stats) = redact_and_envelope(
                &self.config.privacy,
                ingest_run_date,
                &self.tenant_id,
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

    async fn sync_organisation(
        &self,
        client: &XeroApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let body = client.organisation().await?;
        let curated = vec![map_organisation(run_date, run_date, &self.tenant_id, &body)];
        let org_id = record_id(&body, &["OrganisationID"]).unwrap_or_else(|| "unknown".to_string());
        let source_uri = format!("xero://{}/organisation/{}", self.tenant_id, org_id);
        let (raw, stats) = redact_and_envelope(
            &self.config.privacy,
            run_date,
            &self.tenant_id,
            "organisation",
            &org_id,
            &source_uri,
            &body,
        );
        Ok((curated, vec![raw], stats.properties_redacted))
    }

    async fn sync_accounts(
        &self,
        client: &XeroApiClient,
        ctx: &dyn SourceSyncContext,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let ckpt_key = self.checkpoint_key("accounts");
        let if_modified = if discover {
            None
        } else {
            Self::load_last_modified(ctx, &ckpt_key)
        };
        let rows = client.list_accounts(if_modified.as_deref()).await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_account(run_date, run_date, &self.tenant_id, row))
            .collect();
        let (raw_rows, redacted) = self.build_raw_rows(run_date, "accounts", &rows, &["AccountID"]);
        if !discover {
            if let Some(max_updated) = max_updated_date_utc(&rows) {
                Self::store_last_modified(ctx, &ckpt_key, &max_updated)?;
            }
        }
        Ok((curated, raw_rows, redacted))
    }

    async fn sync_contacts(
        &self,
        client: &XeroApiClient,
        ctx: &dyn SourceSyncContext,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let ckpt_key = self.checkpoint_key("contacts");
        let if_modified = if discover {
            None
        } else {
            Self::load_last_modified(ctx, &ckpt_key)
        };
        let rows = client.list_contacts(if_modified.as_deref()).await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_contact(run_date, run_date, &self.tenant_id, row))
            .collect();
        let (raw_rows, redacted) = self.build_raw_rows(run_date, "contacts", &rows, &["ContactID"]);
        if !discover {
            if let Some(max_updated) = max_updated_date_utc(&rows) {
                Self::store_last_modified(ctx, &ckpt_key, &max_updated)?;
            }
        }
        Ok((curated, raw_rows, redacted))
    }

    async fn sync_invoices(
        &self,
        client: &XeroApiClient,
        ctx: &dyn SourceSyncContext,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let ckpt_key = self.checkpoint_key("invoices");
        let if_modified = if discover {
            None
        } else {
            Self::load_last_modified(ctx, &ckpt_key)
        };
        let where_clause = if discover {
            None
        } else {
            Some(self.lookback_where_clause()?)
        };
        let rows = client
            .list_invoices(if_modified.as_deref(), where_clause.as_deref())
            .await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_invoice(run_date, &self.tenant_id, row))
            .collect();
        let line_curated: Vec<Value> = rows
            .iter()
            .flat_map(|row| map_invoice_lines(run_date, &self.tenant_id, row))
            .collect();
        let (invoice_raw, redacted_inv) =
            self.build_raw_rows(run_date, "invoices", &rows, &["InvoiceID"]);
        let mut line_source_rows = Vec::new();
        for invoice in &rows {
            if let Some(items) = invoice.get("LineItems").and_then(|v| v.as_array()) {
                for line in items {
                    line_source_rows.push(line.clone());
                }
            }
        }
        let (line_raw, redacted_lines) = self.build_raw_rows(
            run_date,
            "invoice_lines",
            &line_source_rows,
            &["LineItemID"],
        );
        if !discover {
            if let Some(max_updated) = max_updated_date_utc(&rows) {
                Self::store_last_modified(ctx, &ckpt_key, &max_updated)?;
            }
        }
        Ok((
            curated,
            line_curated,
            invoice_raw,
            line_raw,
            redacted_inv + redacted_lines,
        ))
    }

    async fn sync_payments(
        &self,
        client: &XeroApiClient,
        ctx: &dyn SourceSyncContext,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let ckpt_key = self.checkpoint_key("payments");
        let if_modified = if discover {
            None
        } else {
            Self::load_last_modified(ctx, &ckpt_key)
        };
        let where_clause = if discover {
            None
        } else {
            Some(self.lookback_where_clause()?)
        };
        let rows = client
            .list_payments(if_modified.as_deref(), where_clause.as_deref())
            .await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_payment(run_date, &self.tenant_id, row))
            .collect();
        let (raw_rows, redacted) = self.build_raw_rows(run_date, "payments", &rows, &["PaymentID"]);
        if !discover {
            if let Some(max_updated) = max_updated_date_utc(&rows) {
                Self::store_last_modified(ctx, &ckpt_key, &max_updated)?;
            }
        }
        Ok((curated, raw_rows, redacted))
    }

    async fn sync_bank(
        &self,
        client: &XeroApiClient,
        ctx: &dyn SourceSyncContext,
        run_date: &str,
        discover: bool,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), std::io::Error> {
        let ckpt_key = self.checkpoint_key("bank");
        let if_modified = if discover {
            None
        } else {
            Self::load_last_modified(ctx, &ckpt_key)
        };
        let where_clause = if discover {
            None
        } else {
            Some(self.lookback_where_clause()?)
        };
        let rows = client
            .list_bank_transactions(if_modified.as_deref(), where_clause.as_deref())
            .await?;
        let curated: Vec<Value> = rows
            .iter()
            .map(|row| map_bank_transaction(run_date, &self.tenant_id, row))
            .collect();
        let (raw_rows, redacted) =
            self.build_raw_rows(run_date, "bank_transactions", &rows, &["BankTransactionID"]);
        if !discover {
            if let Some(max_updated) = max_updated_date_utc(&rows) {
                Self::store_last_modified(ctx, &ckpt_key, &max_updated)?;
            }
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
impl DataSource for DataSourceXeroAccountingPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        [
            NAMESPACE_ORGANISATION_SNAPSHOT,
            NAMESPACE_ACCOUNT_SNAPSHOT,
            NAMESPACE_CONTACT_SNAPSHOT,
            NAMESPACE_INVOICE_FACT,
            NAMESPACE_INVOICE_LINE_FACT,
            NAMESPACE_PAYMENT_FACT,
            NAMESPACE_BANK_TRANSACTION_FACT,
            NAMESPACE_ORGANISATION_SOURCE_RAW,
            NAMESPACE_ACCOUNT_SOURCE_RAW,
            NAMESPACE_CONTACT_SOURCE_RAW,
            NAMESPACE_INVOICE_SOURCE_RAW,
            NAMESPACE_INVOICE_LINE_SOURCE_RAW,
            NAMESPACE_PAYMENT_SOURCE_RAW,
            NAMESPACE_BANK_TRANSACTION_SOURCE_RAW,
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
                XeroStream::Organisation => {
                    let (curated, raw, redacted) =
                        self.sync_organisation(&client, &run_date).await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_ORGANISATION_SNAPSHOT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_ORGANISATION_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                XeroStream::Accounts => {
                    let (curated, raw, redacted) = self
                        .sync_accounts(&client, ctx.as_ref(), &run_date, discover)
                        .await?;
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
                XeroStream::Contacts => {
                    let (curated, raw, redacted) = self
                        .sync_contacts(&client, ctx.as_ref(), &run_date, discover)
                        .await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CONTACT_SNAPSHOT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CONTACT_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                XeroStream::Invoices => {
                    let (curated, lines, invoice_raw, line_raw, redacted) = self
                        .sync_invoices(&client, ctx.as_ref(), &run_date, discover)
                        .await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_INVOICE_FACT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_INVOICE_LINE_FACT,
                        &run_date,
                        lines,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_INVOICE_SOURCE_RAW,
                        &run_date,
                        invoice_raw,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_INVOICE_LINE_SOURCE_RAW,
                        &run_date,
                        line_raw,
                    )
                }
                XeroStream::Payments => {
                    let (curated, raw, redacted) = self
                        .sync_payments(&client, ctx.as_ref(), &run_date, discover)
                        .await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PAYMENT_FACT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PAYMENT_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                XeroStream::Bank => {
                    let (curated, raw, redacted) = self
                        .sync_bank(&client, ctx.as_ref(), &run_date, discover)
                        .await?;
                    properties_redacted += redacted;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_BANK_TRANSACTION_FACT,
                        &run_date,
                        curated,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_BANK_TRANSACTION_SOURCE_RAW,
                        &run_date,
                        raw,
                    )
                }
                XeroStream::Health => Ok(()),
            };
            match result {
                Ok(()) => synced.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "Xero stream failed");
                    return Err(err);
                }
            }
        }

        let sync_row = json!({
            "run_date": run_date,
            "tenant_id": self.tenant_id,
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
            tenant_id = %self.tenant_id,
            streams = %synced.join(","),
            elapsed_ms = started.elapsed().as_millis(),
            "Xero sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::streams::{
        NAMESPACE_CONTACT_SNAPSHOT, NAMESPACE_INVOICE_FACT, NAMESPACE_INVOICE_SOURCE_RAW,
        NAMESPACE_SYNC_RUN_DAILY,
    };
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn fact_namespaces_use_merge_by_key() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let plugin = DataSourceXeroAccountingPlugin::new(sample_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        let invoice = contracts
            .iter()
            .find(|c| c.namespace == NAMESPACE_INVOICE_FACT)
            .unwrap();
        assert_eq!(invoice.write_policy, WritePolicy::MergeByKey);
        assert_eq!(invoice.refresh_window, Some(90));
        let payment = contracts
            .iter()
            .find(|c| c.namespace == NAMESPACE_PAYMENT_FACT)
            .unwrap();
        assert_eq!(payment.write_policy, WritePolicy::MergeByKey);
        assert_eq!(payment.refresh_window, Some(90));
    }

    #[test]
    fn namespace_contracts_validate() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let plugin = DataSourceXeroAccountingPlugin::new(sample_config()).unwrap();
        for contract in plugin.source_namespace_contracts() {
            contract.validate().expect("valid contract");
        }
    }

    #[test]
    fn sync_run_contract_uses_run_date_partition() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let contract =
            DataSourceXeroAccountingPlugin::namespace_contract(NAMESPACE_SYNC_RUN_DAILY, 90);
        assert_eq!(contract.write_policy, WritePolicy::ReplacePartition);
        assert_eq!(contract.partition_key[0].dotted(), "run_date");
    }

    #[tokio::test]
    async fn happy_sync() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        set_fixture_dir(fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let plugin = DataSourceXeroAccountingPlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let mut plugin = plugin;
        plugin.sync(ctx.clone()).await.unwrap();

        let invoices = ctx.rows_for_namespace(NAMESPACE_INVOICE_FACT);
        assert!(!invoices.is_empty());
        assert_eq!(
            invoices[0]["invoice_id"],
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        );
        assert!(invoices[0].get("Reference").is_none());

        let contacts = ctx.rows_for_namespace(NAMESPACE_CONTACT_SNAPSHOT);
        assert!(!contacts.is_empty());
        assert!(contacts[0].get("Name").is_none());
        assert!(contacts[0].get("EmailAddress").is_none());

        let raw = ctx.rows_for_namespace(NAMESPACE_INVOICE_SOURCE_RAW);
        assert!(!raw.is_empty());
        assert!(raw[0].get("payload").is_none());
        assert!(raw[0].get("payload_sha256").is_some());
        assert_eq!(raw[0]["tenant_id"], "a1b2c3d4-e5f6-7890-abcd-ef1234567891");

        let runs = ctx.rows_for_namespace(NAMESPACE_SYNC_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");

        clear_fixture_dir();
    }
}
