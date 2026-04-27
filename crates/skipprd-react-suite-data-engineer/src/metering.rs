use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const LOW_BALANCE_THRESHOLD: f64 = 2.0;

static METERING_CLIENT: OnceCell<MeteringClient> = OnceCell::new();

pub fn init_metering(
    accounting_url: Option<String>,
    tokens: Arc<TokenProvider>,
    initial_balance: f64,
) {
    let _ = METERING_CLIENT.set(MeteringClient::new(accounting_url, tokens, initial_balance));
}

pub fn set_metering_run_id(run_id: &str) {
    global_metering().set_run_id(run_id);
}

pub fn set_metering_thread_id(thread_id: &str) {
    global_metering().set_thread_id(thread_id);
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmRequestKind {
    Chat,
    Embed,
}

#[derive(Clone, Debug)]
pub struct LlmUsageRecord {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: String,
    pub kind: LlmRequestKind,
    pub project_id: Option<String>,
    pub thread_id: Option<String>,
    pub prompt_id: Option<String>,
    pub provider_usage: Option<serde_json::Value>,
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub fn report_llm_usage(usage: LlmUsageRecord) {
    let event = UsageEvent::LlmRequest {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        model: usage.model,
        kind: usage.kind,
        project_id: normalize_optional_string(usage.project_id).unwrap_or_default(),
        thread_id: normalize_optional_string(usage.thread_id),
        prompt_id: normalize_optional_string(usage.prompt_id),
        provider_usage: usage.provider_usage,
    };
    let client = global_metering();
    if !client.is_enabled() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let client = global_metering();
            let _ = client.record_batch(&[event]).await;
        });
    }
}

pub fn check_budget() -> Result<(), String> {
    global_metering().budget.check_local()
}

pub fn global_metering() -> &'static MeteringClient {
    static NOOP: once_cell::sync::Lazy<MeteringClient> =
        once_cell::sync::Lazy::new(MeteringClient::noop);
    METERING_CLIENT.get().unwrap_or(&NOOP)
}

#[derive(Clone, Debug, Serialize)]
pub enum UsageEvent {
    FieldsDiscovered {
        count: u64,
        project_id: String,
    },
    TablesSynced {
        count: u64,
        project_id: String,
    },
    ModelsAuthored {
        silver: u64,
        gold: u64,
        project_id: String,
    },
    PlanApproved {
        tasks: u64,
        batches: u64,
        project_id: String,
    },
    RepairCycle {
        cycle: u64,
        project_id: String,
    },
    PipelineRun {
        project_id: String,
    },
    LlmRequest {
        input_tokens: u64,
        output_tokens: u64,
        model: String,
        kind: LlmRequestKind,
        project_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_usage: Option<serde_json::Value>,
    },
    SchemaContractApplied {
        count: u64,
        project_id: String,
    },
    ModelsValidated {
        count: u64,
        project_id: String,
    },
    ModelsPublished {
        count: u64,
        project_id: String,
    },
    CatalogEntryCurated {
        count: u64,
        project_id: String,
    },
    PlanEnriched {
        tasks: u64,
        project_id: String,
    },
    PipelineCompleted {
        project_id: String,
    },
}

impl UsageEvent {
    fn llm_token_totals(&self) -> Option<(u64, u64)> {
        match self {
            Self::LlmRequest {
                input_tokens,
                output_tokens,
                ..
            } => Some((*input_tokens, *output_tokens)),
            _ => None,
        }
    }

    fn explicit_thread_id(&self) -> Option<&str> {
        match self {
            Self::LlmRequest { thread_id, .. } => thread_id.as_deref(),
            _ => None,
        }
    }

    pub fn cost(&self) -> f64 {
        match self {
            Self::FieldsDiscovered { count, .. } => *count as f64 * 0.10,
            Self::TablesSynced { count, .. } => *count as f64 * 1.50,
            Self::ModelsAuthored { silver, gold, .. } => {
                *silver as f64 * 2.00 + *gold as f64 * 3.00
            }
            Self::PlanApproved { .. } => 0.0,
            Self::RepairCycle { .. } => 0.0,
            Self::PipelineRun { .. } => 0.50,
            Self::LlmRequest { .. } => {
                let (input_tokens, output_tokens) = self
                    .llm_token_totals()
                    .expect("llm token totals are present for llm usage events");
                (input_tokens as f64 / 1000.0) * 0.05 + (output_tokens as f64 / 1000.0) * 0.20
            }
            Self::SchemaContractApplied { count, .. } => *count as f64 * 0.50,
            Self::ModelsValidated { count, .. } => *count as f64 * 0.75,
            Self::ModelsPublished { count, .. } => *count as f64 * 1.00,
            Self::CatalogEntryCurated { count, .. } => *count as f64 * 0.25,
            Self::PlanEnriched { tasks, .. } => *tasks as f64 * 0.10,
            Self::PipelineCompleted { .. } => 0.50,
        }
    }

    /// Returns (billing_unit, raw_quantity) tuples for the server API.
    pub fn to_server_records(&self) -> Vec<(&str, f64)> {
        match self {
            Self::FieldsDiscovered { count, .. } => vec![("fields_discovered", *count as f64)],
            Self::TablesSynced { count, .. } => vec![("tables_synced", *count as f64)],
            Self::ModelsAuthored { silver, gold, .. } => {
                let mut v = Vec::new();
                if *silver > 0 {
                    v.push(("models_authored_silver", *silver as f64));
                }
                if *gold > 0 {
                    v.push(("models_authored_gold", *gold as f64));
                }
                v
            }
            Self::LlmRequest { .. } => {
                let (input_tokens, output_tokens) = self
                    .llm_token_totals()
                    .expect("llm token totals are present for llm usage events");
                vec![
                    ("llm_input_tokens", input_tokens as f64),
                    ("llm_output_tokens", output_tokens as f64),
                ]
            }
            Self::PipelineRun { .. } => vec![("pipeline_run", 1.0)],
            Self::PlanApproved { tasks, .. } => vec![("plan_approved", *tasks as f64)],
            Self::RepairCycle { cycle, .. } => vec![("repair_cycle", *cycle as f64)],
            Self::SchemaContractApplied { count, .. } => {
                vec![("schema_contract_applied", *count as f64)]
            }
            Self::ModelsValidated { count, .. } => vec![("models_validated", *count as f64)],
            Self::ModelsPublished { count, .. } => vec![("models_published", *count as f64)],
            Self::CatalogEntryCurated { count, .. } => {
                vec![("catalog_entry_curated", *count as f64)]
            }
            Self::PlanEnriched { tasks, .. } => vec![("plan_enriched", *tasks as f64)],
            Self::PipelineCompleted { .. } => vec![("pipeline_completed", 1.0)],
        }
    }

    pub fn project_id(&self) -> &str {
        match self {
            Self::FieldsDiscovered { project_id, .. }
            | Self::TablesSynced { project_id, .. }
            | Self::ModelsAuthored { project_id, .. }
            | Self::PlanApproved { project_id, .. }
            | Self::RepairCycle { project_id, .. }
            | Self::PipelineRun { project_id, .. }
            | Self::SchemaContractApplied { project_id, .. }
            | Self::ModelsValidated { project_id, .. }
            | Self::ModelsPublished { project_id, .. }
            | Self::CatalogEntryCurated { project_id, .. }
            | Self::PlanEnriched { project_id, .. }
            | Self::PipelineCompleted { project_id, .. }
            | Self::LlmRequest { project_id, .. } => project_id,
        }
    }
}

fn build_server_payload(
    event: &UsageEvent,
    billing_unit: &str,
    quantity: f64,
    run_id: Option<&str>,
    default_thread_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "user_id": "",
        "billing_unit": billing_unit,
        "quantity": quantity,
        "metadata": serde_json::to_value(event).ok(),
        "project_id": event.project_id(),
        "phase": billing_unit,
        "run_id": run_id,
        "thread_id": event
            .explicit_thread_id()
            .or(default_thread_id)
            .map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// TokenProvider — shared, auto-refreshing auth token
// ---------------------------------------------------------------------------

pub struct TokenProvider {
    access_token: Mutex<Option<String>>,
    refresh_token: Option<String>,
    auth_base_url: Option<String>,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct RefreshResponse {
    token: Option<String>,
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

impl TokenProvider {
    pub fn new(
        access_token: Option<String>,
        refresh_token: Option<String>,
        auth_base_url: Option<String>,
    ) -> Self {
        Self {
            access_token: Mutex::new(access_token),
            refresh_token,
            auth_base_url,
            http: reqwest::Client::new(),
        }
    }

    pub fn noop() -> Self {
        Self {
            access_token: Mutex::new(None),
            refresh_token: None,
            auth_base_url: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn get_token(&self) -> Option<String> {
        self.access_token.lock().unwrap().clone()
    }

    async fn refresh(&self) -> Result<(), String> {
        let Some(base_url) = &self.auth_base_url else {
            return Err("no auth base URL configured".into());
        };
        let Some(rt) = &self.refresh_token else {
            return Err("no refresh token available".into());
        };

        let url = format!("{}/auth/refresh", base_url);
        let body = serde_json::json!({ "refresh_token": rt });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("token refresh failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("token refresh returned {}", resp.status()));
        }

        let data: RefreshResponse = resp
            .json()
            .await
            .map_err(|e| format!("token refresh parse failed: {e}"))?;

        if let Some(new_token) = data.token {
            *self.access_token.lock().unwrap() = Some(new_token);
            tracing::info!("Auth token refreshed");
        }

        Ok(())
    }

    /// Send an authenticated request, refreshing the token once on 401/403/500.
    pub async fn send_authenticated(
        &self,
        _client: &reqwest::Client,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, String> {
        let token = self.get_token().unwrap_or_default();
        let resp = build(&token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 || status == 500 {
            if self.refresh().await.is_ok() {
                let new_token = self.get_token().unwrap_or_default();
                return build(&new_token)
                    .send()
                    .await
                    .map_err(|e| format!("retry after refresh failed: {e}"));
            }
        }

        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Budget — in-memory tracker with API-backed verification
// ---------------------------------------------------------------------------

pub struct Budget {
    estimate: Mutex<f64>,
    accounting_url: Option<String>,
    tokens: Arc<TokenProvider>,
    http: Option<reqwest::Client>,
}

impl Budget {
    pub fn new(initial: f64, accounting_url: Option<String>, tokens: Arc<TokenProvider>) -> Self {
        let http = accounting_url.as_ref().map(|_| reqwest::Client::new());
        Self {
            estimate: Mutex::new(initial),
            accounting_url,
            tokens,
            http,
        }
    }

    fn noop() -> Self {
        Self {
            estimate: Mutex::new(f64::MAX),
            accounting_url: None,
            tokens: Arc::new(TokenProvider::noop()),
            http: None,
        }
    }

    pub fn deduct(&self, amount: f64) {
        let mut est = self.estimate.lock().unwrap();
        *est -= amount;
    }

    pub fn local_estimate(&self) -> f64 {
        *self.estimate.lock().unwrap()
    }

    pub fn check_local(&self) -> Result<(), String> {
        if self.local_estimate() <= 0.0 {
            Err("budget exhausted".to_string())
        } else {
            Ok(())
        }
    }

    pub async fn check_and_refresh(&self) -> Result<f64, String> {
        let est = self.local_estimate();
        if est > LOW_BALANCE_THRESHOLD {
            return Ok(est);
        }
        let real = self.fetch_remote_balance().await?;
        *self.estimate.lock().unwrap() = real;
        if real <= 0.0 {
            Err(format!(
                "No balance remaining (${:.2}). Add funds to continue.",
                real
            ))
        } else {
            Ok(real)
        }
    }

    async fn fetch_remote_balance(&self) -> Result<f64, String> {
        let Some(base_url) = &self.accounting_url else {
            return Ok(f64::MAX);
        };
        let Some(client) = &self.http else {
            return Ok(f64::MAX);
        };
        let url = format!("{}/usage/check", base_url);

        let resp = self
            .tokens
            .send_authenticated(client, |token| {
                client
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("balance check parse: {e}"))?;
        Ok(data["balance"].as_f64().unwrap_or(0.0))
    }
}

// ---------------------------------------------------------------------------
// MeteringClient
// ---------------------------------------------------------------------------

pub struct MeteringClient {
    accounting_url: Option<String>,
    tokens: Arc<TokenProvider>,
    http: Option<reqwest::Client>,
    pub budget: Budget,
    run_id: Mutex<Option<String>>,
    thread_id: Mutex<Option<String>>,
}

impl MeteringClient {
    pub fn new(
        accounting_url: Option<String>,
        tokens: Arc<TokenProvider>,
        initial_balance: f64,
    ) -> Self {
        let http = accounting_url.as_ref().map(|_| reqwest::Client::new());
        let budget = Budget::new(initial_balance, accounting_url.clone(), Arc::clone(&tokens));
        Self {
            accounting_url,
            tokens,
            http,
            budget,
            run_id: Mutex::new(None),
            thread_id: Mutex::new(None),
        }
    }

    pub fn noop() -> Self {
        let tokens = Arc::new(TokenProvider::noop());
        Self {
            accounting_url: None,
            tokens: Arc::clone(&tokens),
            http: None,
            budget: Budget::noop(),
            run_id: Mutex::new(None),
            thread_id: Mutex::new(None),
        }
    }

    pub fn set_run_id(&self, id: &str) {
        *self.run_id.lock().unwrap() = Some(id.to_string());
    }

    pub fn set_thread_id(&self, id: &str) {
        *self.thread_id.lock().unwrap() = Some(id.to_string());
    }

    pub fn is_enabled(&self) -> bool {
        self.accounting_url.is_some()
    }

    pub async fn record_batch(&self, events: &[UsageEvent]) -> Result<(), String> {
        let Some(base_url) = &self.accounting_url else {
            return Ok(());
        };
        let Some(client) = &self.http else {
            return Ok(());
        };
        let record_url = format!("{}/usage/record", base_url);
        let run_id = self.run_id.lock().unwrap().clone();
        let thread_id = self.thread_id.lock().unwrap().clone();

        for event in events {
            self.budget.deduct(event.cost());

            for (billing_unit, quantity) in event.to_server_records() {
                let payload = build_server_payload(
                    event,
                    billing_unit,
                    quantity,
                    run_id.as_deref(),
                    thread_id.as_deref(),
                );

                let resp = self
                    .tokens
                    .send_authenticated(client, |token| {
                        client
                            .post(&record_url)
                            .json(&payload)
                            .header("Authorization", format!("Bearer {}", token))
                    })
                    .await;

                if let Err(e) = resp {
                    tracing::warn!(billing_unit = billing_unit, error = %e, "Failed to record usage event");
                }
            }
        }

        self.budget.check_and_refresh().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn llm_request_event(input_tokens: u64, output_tokens: u64) -> UsageEvent {
        UsageEvent::LlmRequest {
            input_tokens,
            output_tokens,
            model: "gpt-4o".into(),
            kind: LlmRequestKind::Chat,
            project_id: "test".into(),
            thread_id: Some("thread-123".into()),
            prompt_id: Some("prompt.test".into()),
            provider_usage: Some(json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "output_tokens_details": {
                    "reasoning_tokens": output_tokens / 2
                }
            })),
        }
    }

    #[test]
    fn cost_calculations() {
        let events = vec![
            UsageEvent::FieldsDiscovered {
                count: 100,
                project_id: "test".into(),
            },
            UsageEvent::TablesSynced {
                count: 10,
                project_id: "test".into(),
            },
            UsageEvent::ModelsAuthored {
                silver: 5,
                gold: 3,
                project_id: "test".into(),
            },
            UsageEvent::PlanApproved {
                tasks: 2,
                batches: 1,
                project_id: "test".into(),
            },
            UsageEvent::RepairCycle {
                cycle: 1,
                project_id: "test".into(),
            },
            UsageEvent::PipelineRun {
                project_id: "test".into(),
            },
            llm_request_event(1000, 500),
        ];
        let costs: Vec<f64> = events.iter().map(|e| e.cost()).collect();
        assert!((costs[0] - 10.0).abs() < f64::EPSILON); // 100 fields * $0.10
        assert!((costs[1] - 15.0).abs() < f64::EPSILON); // 10 tables * $1.50
        assert!((costs[2] - 19.0).abs() < f64::EPSILON); // 5*$2.00 + 3*$3.00
        assert!((costs[3] - 0.0).abs() < f64::EPSILON);
        assert!((costs[4] - 0.0).abs() < f64::EPSILON);
        assert!((costs[5] - 0.50).abs() < f64::EPSILON);
        assert!((costs[6] - 0.15).abs() < f64::EPSILON); // 1K*$0.05 + 0.5K*$0.20
    }

    #[test]
    fn cost_calculations_new_events() {
        let events = vec![
            UsageEvent::SchemaContractApplied {
                count: 4,
                project_id: "t".into(),
            },
            UsageEvent::ModelsValidated {
                count: 6,
                project_id: "t".into(),
            },
            UsageEvent::ModelsPublished {
                count: 3,
                project_id: "t".into(),
            },
            UsageEvent::CatalogEntryCurated {
                count: 2,
                project_id: "t".into(),
            },
            UsageEvent::PlanEnriched {
                tasks: 10,
                project_id: "t".into(),
            },
            UsageEvent::PipelineCompleted {
                project_id: "t".into(),
            },
        ];
        let costs: Vec<f64> = events.iter().map(|e| e.cost()).collect();
        assert!((costs[0] - 2.0).abs() < f64::EPSILON); // 4 * $0.50
        assert!((costs[1] - 4.5).abs() < f64::EPSILON); // 6 * $0.75
        assert!((costs[2] - 3.0).abs() < f64::EPSILON); // 3 * $1.00
        assert!((costs[3] - 0.5).abs() < f64::EPSILON); // 2 * $0.25
        assert!((costs[4] - 1.0).abs() < f64::EPSILON); // 10 * $0.10
        assert!((costs[5] - 0.5).abs() < f64::EPSILON); // flat $0.50
    }

    #[test]
    fn to_server_records_expands_compound_events() {
        let ev = UsageEvent::ModelsAuthored {
            silver: 3,
            gold: 2,
            project_id: "p".into(),
        };
        let records = ev.to_server_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], ("models_authored_silver", 3.0));
        assert_eq!(records[1], ("models_authored_gold", 2.0));

        let ev = UsageEvent::LlmRequest {
            input_tokens: 5000,
            output_tokens: 1000,
            model: "m".into(),
            kind: LlmRequestKind::Embed,
            project_id: "p".into(),
            thread_id: Some("thread-embed".into()),
            prompt_id: None,
            provider_usage: Some(json!({
                "prompt_tokens": 5000,
                "total_tokens": 5000
            })),
        };
        let records = ev.to_server_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], ("llm_input_tokens", 5000.0));
        assert_eq!(records[1], ("llm_output_tokens", 1000.0));
    }

    #[test]
    fn to_server_records_simple_events() {
        let ev = UsageEvent::TablesSynced {
            count: 10,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("tables_synced", 10.0)]);

        let ev = UsageEvent::FieldsDiscovered {
            count: 50,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("fields_discovered", 50.0)]);

        let ev = UsageEvent::PipelineRun {
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("pipeline_run", 1.0)]);
    }

    #[test]
    fn budget_deduct_and_check() {
        let tokens = Arc::new(TokenProvider::noop());
        let budget = Budget::new(10.0, None, tokens);
        assert!(budget.check_local().is_ok());
        assert!((budget.local_estimate() - 10.0).abs() < f64::EPSILON);

        budget.deduct(8.0);
        assert!(budget.check_local().is_ok());
        assert!((budget.local_estimate() - 2.0).abs() < f64::EPSILON);

        budget.deduct(3.0);
        assert!(budget.check_local().is_err());
        assert!(budget.local_estimate() < 0.0);
    }

    #[test]
    fn to_server_records_new_events() {
        let ev = UsageEvent::SchemaContractApplied {
            count: 3,
            project_id: "p".into(),
        };
        assert_eq!(
            ev.to_server_records(),
            vec![("schema_contract_applied", 3.0)]
        );

        let ev = UsageEvent::ModelsValidated {
            count: 5,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("models_validated", 5.0)]);

        let ev = UsageEvent::ModelsPublished {
            count: 2,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("models_published", 2.0)]);

        let ev = UsageEvent::CatalogEntryCurated {
            count: 1,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("catalog_entry_curated", 1.0)]);

        let ev = UsageEvent::PlanEnriched {
            tasks: 8,
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("plan_enriched", 8.0)]);

        let ev = UsageEvent::PipelineCompleted {
            project_id: "p".into(),
        };
        assert_eq!(ev.to_server_records(), vec![("pipeline_completed", 1.0)]);
    }

    #[test]
    fn models_authored_zero_silver_omitted() {
        let ev = UsageEvent::ModelsAuthored {
            silver: 0,
            gold: 5,
            project_id: "p".into(),
        };
        let records = ev.to_server_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0], ("models_authored_gold", 5.0));
    }

    #[test]
    fn llm_request_metadata_preserves_provider_usage_breakdown() {
        let ev = llm_request_event(320, 120);
        let metadata = serde_json::to_value(&ev).expect("llm event should serialize");

        let provider_usage = &metadata["LlmRequest"]["provider_usage"];
        assert_eq!(provider_usage["input_tokens"], 320);
        assert_eq!(provider_usage["output_tokens"], 120);
        assert_eq!(
            provider_usage["output_tokens_details"]["reasoning_tokens"],
            60
        );
    }

    #[test]
    fn build_server_payload_prefers_event_thread_and_project_context() {
        let ev = UsageEvent::LlmRequest {
            input_tokens: 88,
            output_tokens: 12,
            model: "gpt-4o-mini".into(),
            kind: LlmRequestKind::Chat,
            project_id: "project-alpha".into(),
            thread_id: Some("thread-from-event".into()),
            prompt_id: Some("prompt.alpha".into()),
            provider_usage: Some(json!({
                "input_tokens": 88,
                "output_tokens": 12
            })),
        };

        let payload = build_server_payload(
            &ev,
            "llm_input_tokens",
            88.0,
            Some("run-42"),
            Some("thread-from-client"),
        );

        assert_eq!(payload["project_id"], "project-alpha");
        assert_eq!(payload["run_id"], "run-42");
        assert_eq!(payload["thread_id"], "thread-from-event");
        assert_eq!(
            payload["metadata"]["LlmRequest"]["prompt_id"],
            "prompt.alpha"
        );
    }
}
