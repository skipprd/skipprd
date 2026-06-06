use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
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

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
use tracing::{info, warn};

use crate::hubspot_api::{prop_str, HubspotApiClient, FIXTURE_ENV};
use crate::privacy::{path_without_query, PrivacyConfig, PrivacyStats};
use crate::streams::{
    resolve_streams, HubspotStream, StreamProfile, NAMESPACE_COMPANY_SNAPSHOT,
    NAMESPACE_DEAL_SNAPSHOT, NAMESPACE_EVENT_FACT, NAMESPACE_FORM_DIM, NAMESPACE_LANDING_PAGE_DIM,
    NAMESPACE_PIPELINE_STAGE_DIM, NAMESPACE_PORTAL_SNAPSHOT, NAMESPACE_SYNC_RUN_DAILY,
};

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceHubspotCrmPluginConfig {
    pub hub_id: String,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HubspotIngestCheckpoint {
    last_occurred_at: String,
}

fn default_min_query_interval_ms() -> u64 {
    200
}

impl DataSourceHubspotCrmPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.hub_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "hub_id is required",
            ));
        }
        Ok(())
    }
}

pub struct DataSourceHubspotCrmPlugin {
    config: DataSourceHubspotCrmPluginConfig,
    hub_id: String,
}

impl DataSourceHubspotCrmPlugin {
    pub fn new(config: DataSourceHubspotCrmPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self {
            hub_id: config.hub_id.trim().to_string(),
            config,
        })
    }

    async fn api_client(&self) -> Result<HubspotApiClient, std::io::Error> {
        let http = RetryableHttpClient::new(RetryConfig::default());
        if let Some(token) = self
            .config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(HubspotApiClient::new(
                http,
                self.hub_id.clone(),
                token.trim().to_string(),
                self.config.min_query_interval_ms,
            ));
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(HubspotApiClient::new(
                http,
                self.hub_id.clone(),
                "fixture".into(),
                self.config.min_query_interval_ms,
            ));
        }
        let token_url = self.config.oauth_token_url.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "HubSpot requires oauth_refresh_token + client credentials or SKIPPR_HUBSPOT_FIXTURE_DIR",
            )
        })?;
        let client_id = self.config.oauth_client_id.as_deref().unwrap_or("");
        let client_secret = self.config.oauth_client_secret.as_deref().unwrap_or("");
        let refresh = self.config.oauth_refresh_token.as_deref().unwrap_or("");
        HubspotApiClient::from_oauth(
            http,
            self.hub_id.clone(),
            token_url,
            client_id,
            client_secret,
            refresh,
            self.config.min_query_interval_ms,
        )
        .await
    }

    fn selected_streams(&self) -> Vec<HubspotStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<HubspotStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn redact(&self, row: &mut Value) -> PrivacyStats {
        self.config.privacy.redact_row(row)
    }

    fn event_row(
        &self,
        ingest_run_date: &str,
        event_type: &str,
        occurred_at: &str,
        ids: Value,
        attribution: Value,
    ) -> Value {
        let mut row = json!({
            "event_id": format!("{}:{}:{}", event_type, occurred_at, serde_json::to_string(&ids).unwrap_or_default()),
            "event_type": event_type,
            "occurred_at": occurred_at,
            "hub_id": self.hub_id,
            "ingest_run_date": ingest_run_date,
        });
        if let (Some(base), Some(extra)) = (row.as_object_mut(), ids.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        if let (Some(base), Some(extra)) = (row.as_object_mut(), attribution.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        self.redact(&mut row);
        row
    }

    fn events_from_deal_history(
        &self,
        ingest_run_date: &str,
        deal_id: &str,
        history: &Value,
        attribution: Value,
    ) -> Vec<Value> {
        let mut events = Vec::new();
        if let Some(created) = history
            .pointer("/propertiesWithHistory/createdate")
            .and_then(|v| v.as_array())
            .and_then(|entries| entries.first())
        {
            let occurred = created
                .get("timestamp")
                .or_else(|| created.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !occurred.is_empty() {
                events.push(self.event_row(
                    ingest_run_date,
                    "deal.created",
                    occurred,
                    json!({ "deal_id": deal_id }),
                    attribution.clone(),
                ));
            }
        }
        if let Some(stages) = history
            .pointer("/propertiesWithHistory/dealstage")
            .and_then(|v| v.as_array())
        {
            for entry in stages {
                let occurred = entry
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if occurred.is_empty() {
                    continue;
                }
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                events.push(self.event_row(
                    ingest_run_date,
                    "deal.stage_changed",
                    occurred,
                    json!({ "deal_id": deal_id }),
                    json!({
                        "property_name": "dealstage",
                        "value_string": value,
                    }),
                ));
            }
        }
        events
    }

    fn events_from_contact_history(
        &self,
        ingest_run_date: &str,
        contact_id: &str,
        company_id: Option<String>,
        history: &Value,
        attribution: Value,
    ) -> Vec<Value> {
        let mut events = Vec::new();
        if let Some(created) = history
            .pointer("/propertiesWithHistory/createdate")
            .and_then(|v| v.as_array())
            .and_then(|entries| entries.first())
        {
            let occurred = created
                .get("timestamp")
                .or_else(|| created.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !occurred.is_empty() {
                events.push(self.event_row(
                    ingest_run_date,
                    "contact.created",
                    occurred,
                    json!({
                        "contact_id": contact_id,
                        "company_id": company_id,
                    }),
                    attribution.clone(),
                ));
            }
        }
        if let Some(stages) = history
            .pointer("/propertiesWithHistory/lifecyclestage")
            .and_then(|v| v.as_array())
        {
            for entry in stages {
                let occurred = entry
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if occurred.is_empty() {
                    continue;
                }
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                events.push(self.event_row(
                    ingest_run_date,
                    "contact.lifecycle_changed",
                    occurred,
                    json!({
                        "contact_id": contact_id,
                        "company_id": company_id,
                    }),
                    json!({
                        "property_name": "lifecyclestage",
                        "value_string": value,
                    }),
                ));
            }
        }
        events
    }

    fn filter_events_after_watermark(events: Vec<Value>, watermark: Option<&str>) -> Vec<Value> {
        let Some(wm) = watermark else {
            return events;
        };
        events
            .into_iter()
            .filter(|e| {
                e.get("occurred_at")
                    .and_then(|v| v.as_str())
                    .map(|o| o > wm)
                    .unwrap_or(true)
            })
            .collect()
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        match namespace {
            NAMESPACE_EVENT_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("event_id")],
                cursor: Some(FieldPath::single("occurred_at")),
                partition_key: vec![FieldPath::single("occurred_at")],
                write_policy: WritePolicy::Append,
                refresh_window: None,
                description: "HubSpot touchpoint events".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_PORTAL_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("hub_id"), FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot portal snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_DEAL_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("deal_id"), FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot deal snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_COMPANY_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("company_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot company snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_PIPELINE_STAGE_DIM => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("pipeline_id"),
                    FieldPath::single("stage_id"),
                ],
                cursor: None,
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot pipeline stages".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_FORM_DIM => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("form_id")],
                cursor: None,
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot forms".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_LANDING_PAGE_DIM => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("page_id")],
                cursor: None,
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot landing pages".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_SYNC_RUN_DAILY => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("hub_id"), FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "HubSpot sync health".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            other => panic!("unknown hubspot namespace: {other}"),
        }
    }

    fn ingest_checkpoint_key(&self) -> String {
        format!("hubspot:{}:{}", self.hub_id, NAMESPACE_EVENT_FACT)
    }

    fn load_last_occurred_at(ctx: &dyn SourceSyncContext, key: &str) -> Option<String> {
        load_checkpoint_payload::<HubspotIngestCheckpoint>(ctx, key).map(|cp| cp.last_occurred_at)
    }

    fn store_last_occurred_at(
        ctx: &dyn SourceSyncContext,
        key: &str,
        occurred_at: &str,
    ) -> Result<(), std::io::Error> {
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &HubspotIngestCheckpoint {
                last_occurred_at: occurred_at.to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    fn submit_events(
        &self,
        ctx: &dyn SourceSyncContext,
        events: Vec<Value>,
    ) -> Result<Option<String>, std::io::Error> {
        if events.is_empty() {
            return Ok(None);
        }
        let mut max_occurred: Option<String> = None;
        let mut by_day: std::collections::HashMap<String, Vec<Value>> =
            std::collections::HashMap::new();
        for row in events {
            let occurred = row
                .get("occurred_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if occurred.is_empty() {
                continue;
            }
            let day = occurred.get(0..10).unwrap_or(&occurred).to_string();
            if max_occurred
                .as_ref()
                .map(|m| occurred.as_str() > m.as_str())
                .unwrap_or(true)
            {
                max_occurred = Some(occurred.clone());
            }
            by_day.entry(day).or_default().push(row);
        }
        for (day, batch) in by_day {
            self.submit_namespace(ctx, NAMESPACE_EVENT_FACT, &day, batch)?;
        }
        Ok(max_occurred)
    }

    fn submit_namespace(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        partition: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, partition.to_string()),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("hubspot://{}/{}", self.hub_id, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn sync_crm(
        &self,
        client: &HubspotApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>), std::io::Error> {
        let deal_props: Vec<&str> = self
            .config
            .privacy
            .deal_property_allowlist()
            .map(|list| list.to_vec())
            .unwrap_or_else(|| {
                vec![
                    "dealname",
                    "amount",
                    "dealstage",
                    "pipeline",
                    "createdate",
                    "closedate",
                ]
            });
        let contact_props: Vec<&str> = self
            .config
            .privacy
            .contact_property_allowlist()
            .map(|list| list.to_vec())
            .unwrap_or_else(|| vec!["lifecyclestage", "createdate", "hs_analytics_source"]);

        let mut events = Vec::new();
        let mut deal_snaps = Vec::new();
        let mut company_snaps = Vec::new();
        let mut stage_dims = Vec::new();
        let mut portal_snaps = Vec::new();

        let account = client.account_details().await?;
        portal_snaps.push({
            let mut row = json!({
                "run_date": run_date,
                "hub_id": self.hub_id,
                "timezone": account.get("timeZone").and_then(|v| v.as_str()),
                "currency": account.get("currency").and_then(|v| v.as_str()),
                "account_type": account.get("accountType").and_then(|v| v.as_str()),
            });
            self.redact(&mut row);
            row
        });

        let pipelines = client.deal_pipelines().await?;
        if let Some(results) = pipelines.get("results").and_then(|v| v.as_array()) {
            for pipe in results {
                let pipeline_id = pipe.get("id").and_then(|v| v.as_str()).unwrap_or("default");
                if let Some(stages) = pipe.get("stages").and_then(|v| v.as_array()) {
                    for stage in stages {
                        let mut row = json!({
                            "run_date": run_date,
                            "hub_id": self.hub_id,
                            "pipeline_id": pipeline_id,
                            "stage_id": stage.get("id").and_then(|v| v.as_str()),
                            "label": stage.get("label").and_then(|v| v.as_str()),
                            "display_order": stage.get("displayOrder").and_then(|v| v.as_i64()),
                        });
                        self.redact(&mut row);
                        stage_dims.push(row);
                    }
                }
            }
        }

        let deals_body = client.search_deals(&deal_props).await?;
        if let Some(results) = deals_body.get("results").and_then(|v| v.as_array()) {
            for deal in results {
                let deal_id = deal.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let props = deal.get("properties").cloned().unwrap_or(json!({}));
                let attribution = attribution_from_props(&props);
                if !deal_id.is_empty() {
                    let history = client
                        .deal_with_property_history(deal_id, &["dealstage", "createdate"])
                        .await?;
                    events.extend(self.events_from_deal_history(
                        run_date,
                        deal_id,
                        &history,
                        attribution.clone(),
                    ));
                }
                let mut snap = json!({
                    "run_date": run_date,
                    "hub_id": self.hub_id,
                    "deal_id": deal_id,
                    "amount": prop_str(&props, "amount"),
                    "currency": account.get("currency").and_then(|v| v.as_str()),
                    "stage_id": prop_str(&props, "dealstage"),
                    "pipeline_id": prop_str(&props, "pipeline"),
                    "close_date": prop_str(&props, "closedate"),
                    "create_date": prop_str(&props, "createdate"),
                    "is_closed_won": prop_str(&props, "hs_is_closed_won"),
                    "owner_id": prop_str(&props, "hubspot_owner_id"),
                });
                if let Some(obj) = snap.as_object_mut() {
                    for (k, v) in attribution_from_props(&props)
                        .as_object()
                        .unwrap_or(&serde_json::Map::new())
                    {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                self.redact(&mut snap);
                deal_snaps.push(snap);
            }
        }

        let contacts_body = client.search_contacts(&contact_props).await?;
        if let Some(results) = contacts_body.get("results").and_then(|v| v.as_array()) {
            for contact in results {
                let contact_id = contact.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let props = contact.get("properties").cloned().unwrap_or(json!({}));
                let company_id = prop_str(&props, "associatedcompanyid");
                if !contact_id.is_empty() {
                    let history = client
                        .contact_with_property_history(
                            contact_id,
                            &["lifecyclestage", "createdate"],
                        )
                        .await?;
                    events.extend(self.events_from_contact_history(
                        run_date,
                        contact_id,
                        company_id,
                        &history,
                        attribution_from_props(&props),
                    ));
                }
            }
        }

        let companies_body = client
            .search_companies(&[
                "domain",
                "industry",
                "numberofemployees",
                "country",
                "lifecyclestage",
                "createdate",
            ])
            .await?;
        if let Some(results) = companies_body.get("results").and_then(|v| v.as_array()) {
            for company in results {
                let company_id = company.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let props = company.get("properties").cloned().unwrap_or(json!({}));
                let created =
                    prop_str(&props, "createdate").unwrap_or_else(|| run_date.to_string());
                events.push(self.event_row(
                    run_date,
                    "company.created",
                    &created,
                    json!({ "company_id": company_id }),
                    json!({}),
                ));
                let mut snap = json!({
                    "run_date": run_date,
                    "hub_id": self.hub_id,
                    "company_id": company_id,
                    "domain": prop_str(&props, "domain"),
                    "industry": prop_str(&props, "industry"),
                    "employee_range": prop_str(&props, "numberofemployees"),
                    "country": prop_str(&props, "country"),
                    "lifecyclestage": prop_str(&props, "lifecyclestage"),
                });
                self.redact(&mut snap);
                company_snaps.push(snap);
            }
        }

        Ok((events, portal_snaps, stage_dims, deal_snaps, company_snaps))
    }

    async fn sync_marketing(
        &self,
        client: &HubspotApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let mut events = Vec::new();
        let mut forms = Vec::new();
        let forms_body = client.list_forms().await?;
        if let Some(results) = forms_body.get("results").and_then(|v| v.as_array()) {
            for form in results {
                let form_id = form.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let mut dim = json!({
                    "run_date": run_date,
                    "hub_id": self.hub_id,
                    "form_id": form_id,
                    "name": form.get("name").and_then(|v| v.as_str()),
                });
                self.redact(&mut dim);
                forms.push(dim);
            }
        }
        let emails_body = client.list_marketing_emails().await?;
        if let Some(results) = emails_body.get("results").and_then(|v| v.as_array()) {
            for email in results {
                let email_id = email.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let occurred = email
                    .get("publishedAt")
                    .or_else(|| email.get("createdAt"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(run_date);
                events.push(self.event_row(
                    run_date,
                    "email.published",
                    occurred,
                    json!({ "email_id": email_id }),
                    json!({}),
                ));
            }
        }
        Ok((events, forms))
    }

    async fn sync_service(
        &self,
        client: &HubspotApiClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut events = Vec::new();
        let ticket_props = [
            "hs_pipeline",
            "hs_pipeline_stage",
            "hs_ticket_priority",
            "source_type",
            "createdate",
            "closed_date",
            "hs_time_to_close",
        ];
        let body = client.search_tickets(&ticket_props).await?;
        if let Some(results) = body.get("results").and_then(|v| v.as_array()) {
            for ticket in results {
                let ticket_id = ticket.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let props = ticket.get("properties").cloned().unwrap_or(json!({}));
                let created =
                    prop_str(&props, "createdate").unwrap_or_else(|| run_date.to_string());
                let company_id = ticket
                    .pointer("/associations/companies/results/0/id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let contact_id = ticket
                    .pointer("/associations/contacts/results/0/id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                events.push(self.event_row(
                    run_date,
                    "ticket.created",
                    &created,
                    json!({
                        "ticket_id": ticket_id,
                        "company_id": company_id,
                        "contact_id": contact_id,
                    }),
                    json!({
                        "property_name": "hs_ticket_priority",
                        "value_string": prop_str(&props, "hs_ticket_priority"),
                    }),
                ));
                if prop_str(&props, "closed_date").is_some() {
                    events.push(self.event_row(
                        run_date,
                        "ticket.closed",
                        &prop_str(&props, "closed_date").unwrap_or_else(|| created.clone()),
                        json!({
                            "ticket_id": ticket_id,
                            "company_id": company_id,
                        }),
                        json!({}),
                    ));
                }
            }
        }
        Ok(events)
    }

    async fn sync_onsite(
        &self,
        client: &HubspotApiClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let events = Vec::new();
        let mut pages = Vec::new();
        let body = client.list_landing_pages().await?;
        if let Some(results) = body.get("results").and_then(|v| v.as_array()) {
            for page in results {
                let page_id = page.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let url = page.get("url").and_then(|v| v.as_str());
                let mut dim = json!({
                    "run_date": run_date,
                    "hub_id": self.hub_id,
                    "page_id": page_id,
                    "slug": page.get("slug").and_then(|v| v.as_str()),
                    "url_path": path_without_query(url),
                    "state": page.get("currentState").and_then(|v| v.as_str()),
                    "campaign_id": page.get("campaign").and_then(|v| v.as_str()),
                });
                self.redact(&mut dim);
                pages.push(dim);
            }
        }
        Ok((events, pages))
    }
}

fn attribution_from_props(props: &Value) -> Value {
    json!({
        "utm_source": prop_str(props, "utm_source"),
        "utm_medium": prop_str(props, "utm_medium"),
        "utm_campaign": prop_str(props, "utm_campaign"),
        "utm_content": prop_str(props, "utm_content"),
        "utm_term": prop_str(props, "utm_term"),
        "hs_analytics_source": prop_str(props, "hs_analytics_source"),
        "hs_analytics_source_data_1": prop_str(props, "hs_analytics_source_data_1"),
        "hs_analytics_source_data_2": prop_str(props, "hs_analytics_source_data_2"),
    })
}

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[async_trait]
impl DataSource for DataSourceHubspotCrmPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        [
            NAMESPACE_EVENT_FACT,
            NAMESPACE_PORTAL_SNAPSHOT,
            NAMESPACE_PIPELINE_STAGE_DIM,
            NAMESPACE_DEAL_SNAPSHOT,
            NAMESPACE_COMPANY_SNAPSHOT,
            NAMESPACE_FORM_DIM,
            NAMESPACE_LANDING_PAGE_DIM,
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
        let checkpoint_key = self.ingest_checkpoint_key();
        let watermark = if discover {
            None
        } else {
            Self::load_last_occurred_at(ctx.as_ref(), &checkpoint_key)
        };
        let mut all_events: Vec<Value> = Vec::new();
        let mut synced: Vec<&str> = Vec::new();
        let mut properties_redacted: u64 = 0;

        for stream in &streams {
            let result: Result<(), std::io::Error> = match stream {
                HubspotStream::Crm => {
                    let (events, portal, stages, deals, companies) =
                        self.sync_crm(&client, &run_date).await?;
                    properties_redacted += events.len() as u64;
                    all_events.extend(events);
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PORTAL_SNAPSHOT,
                        &run_date,
                        portal,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PIPELINE_STAGE_DIM,
                        &run_date,
                        stages,
                    )?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_DEAL_SNAPSHOT, &run_date, deals)?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_COMPANY_SNAPSHOT,
                        &run_date,
                        companies,
                    )?;
                    Ok(())
                }
                HubspotStream::Marketing => {
                    let (events, forms) = self.sync_marketing(&client, &run_date).await?;
                    all_events.extend(events);
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_FORM_DIM, &run_date, forms)?;
                    Ok(())
                }
                HubspotStream::Service => {
                    let events = self.sync_service(&client, &run_date).await?;
                    all_events.extend(events);
                    Ok(())
                }
                HubspotStream::Onsite => {
                    let (events, pages) = self.sync_onsite(&client, &run_date).await?;
                    all_events.extend(events);
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_LANDING_PAGE_DIM,
                        &run_date,
                        pages,
                    )?;
                    Ok(())
                }
            };
            match result {
                Ok(()) => synced.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "HubSpot stream failed");
                    return Err(err);
                }
            }
        }

        let all_events = Self::filter_events_after_watermark(all_events, watermark.as_deref());
        if let Some(max_occurred) = self.submit_events(ctx.as_ref(), all_events)? {
            if !discover {
                Self::store_last_occurred_at(ctx.as_ref(), &checkpoint_key, &max_occurred)?;
            }
        }

        let sync_row = json!({
            "run_date": run_date,
            "hub_id": self.hub_id,
            "status": "ok",
            "streams_synced": synced.join(","),
            "properties_redacted": properties_redacted,
            "elapsed_ms": started.elapsed().as_millis() as i64,
        });
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_SYNC_RUN_DAILY,
            &run_date,
            vec![sync_row],
        )?;

        info!(
            hub_id = %self.hub_id,
            streams = %synced.join(","),
            elapsed_ms = started.elapsed().as_millis(),
            "HubSpot CRM sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;
    use crate::streams::NAMESPACE_EVENT_FACT;
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };
    use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn fixture_sync_emits_events_without_pii() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        set_fixture_dir(fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let plugin = DataSourceHubspotCrmPlugin::new(sample_config()).unwrap();
        let ctx = std::sync::Arc::new(RecordingSyncContext::default());
        let mut plugin = plugin;
        plugin.sync(ctx.clone()).await.unwrap();

        let events = ctx.rows_for_namespace(NAMESPACE_EVENT_FACT);
        assert!(!events.is_empty());
        assert!(events.iter().all(|e| e.get("email").is_none()));
        assert!(events
            .iter()
            .any(|e| e.get("event_type").and_then(|v| v.as_str()) == Some("deal.created")));

        clear_fixture_dir();
    }
}
