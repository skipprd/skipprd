use serde_json::Value;
use skippr_plugin_shared_api_source::{OAuth2RefreshTokenAuth, RetryableHttpClient};

pub const FIXTURE_ENV: &str = "SKIPPR_HUBSPOT_FIXTURE_DIR";
const API_BASE: &str = "https://api.hubapi.com";

#[derive(Clone)]
pub struct HubspotApiClient {
    pub http: RetryableHttpClient,
    pub hub_id: String,
    access_token: String,
    pub min_interval_ms: u64,
}

impl HubspotApiClient {
    pub fn new(
        http: RetryableHttpClient,
        hub_id: String,
        access_token: String,
        min_interval_ms: u64,
    ) -> Self {
        Self {
            http,
            hub_id,
            access_token,
            min_interval_ms,
        }
    }

    pub async fn from_oauth(
        http: RetryableHttpClient,
        hub_id: String,
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
        min_interval_ms: u64,
    ) -> Result<Self, std::io::Error> {
        let oauth = OAuth2RefreshTokenAuth::new(token_url, client_id, client_secret, refresh_token);
        let access_token = oauth.refresh().await.map_err(std::io::Error::other)?;
        Ok(Self::new(http, hub_id, access_token, min_interval_ms))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.access_token)
    }

    fn fixture_path(dir: &str, name: &str) -> Option<Value> {
        let path = format!("{}/{}", dir.trim_end_matches('/'), name);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn get_json(&self, path: &str, fixture: &str) -> Result<Value, std::io::Error> {
        if let Ok(dir) = std::env::var(FIXTURE_ENV) {
            if !dir.trim().is_empty() {
                if let Some(body) = Self::fixture_path(&dir, fixture) {
                    return Ok(body);
                }
            }
        }
        let url = format!("{API_BASE}{path}");
        let response = self
            .http
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(std::io::Error::other)?;
        Self::parse_response(response).await
    }

    async fn post_json(
        &self,
        path: &str,
        fixture: &str,
        body: Value,
    ) -> Result<Value, std::io::Error> {
        if let Ok(dir) = std::env::var(FIXTURE_ENV) {
            if !dir.trim().is_empty() {
                if let Some(body) = Self::fixture_path(&dir, fixture) {
                    return Ok(body);
                }
            }
        }
        let url = format!("{API_BASE}{path}");
        let response = self
            .http
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(std::io::Error::other)?;
        Self::parse_response(response).await
    }

    async fn parse_response(response: reqwest::Response) -> Result<Value, std::io::Error> {
        let status = response.status();
        let text = response.text().await.map_err(std::io::Error::other)?;
        if !status.is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HubSpot HTTP {status}: {text}"),
            ));
        }
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub async fn account_details(&self) -> Result<Value, std::io::Error> {
        self.get_json("/account-info/v3/details", "account.json")
            .await
    }

    pub async fn deal_pipelines(&self) -> Result<Value, std::io::Error> {
        self.get_json("/crm/v3/pipelines/deals", "deal_pipelines.json")
            .await
    }

    pub async fn search_deals(&self, properties: &[&str]) -> Result<Value, std::io::Error> {
        let props: Vec<String> = properties.iter().map(|s| (*s).to_string()).collect();
        self.post_json(
            "/crm/v3/objects/deals/search",
            "deals.json",
            serde_json::json!({
                "filterGroups": [],
                "properties": props,
                "limit": 100
            }),
        )
        .await
    }

    pub async fn search_contacts(&self, properties: &[&str]) -> Result<Value, std::io::Error> {
        let props: Vec<String> = properties.iter().map(|s| (*s).to_string()).collect();
        self.post_json(
            "/crm/v3/objects/contacts/search",
            "contacts.json",
            serde_json::json!({
                "filterGroups": [],
                "properties": props,
                "limit": 100
            }),
        )
        .await
    }

    pub async fn search_companies(&self, properties: &[&str]) -> Result<Value, std::io::Error> {
        let props: Vec<String> = properties.iter().map(|s| (*s).to_string()).collect();
        self.post_json(
            "/crm/v3/objects/companies/search",
            "companies.json",
            serde_json::json!({
                "filterGroups": [],
                "properties": props,
                "limit": 100
            }),
        )
        .await
    }

    pub async fn list_forms(&self) -> Result<Value, std::io::Error> {
        self.get_json("/marketing/v3/forms", "forms.json").await
    }

    pub async fn search_tickets(&self, properties: &[&str]) -> Result<Value, std::io::Error> {
        let props: Vec<String> = properties.iter().map(|s| (*s).to_string()).collect();
        self.post_json(
            "/crm/v3/objects/tickets/search",
            "tickets.json",
            serde_json::json!({
                "filterGroups": [],
                "properties": props,
                "limit": 100
            }),
        )
        .await
    }

    pub async fn list_landing_pages(&self) -> Result<Value, std::io::Error> {
        self.get_json("/cms/v3/pages/landing-pages?limit=50", "landing_pages.json")
            .await
    }

    pub async fn deal_with_property_history(
        &self,
        deal_id: &str,
        properties: &[&str],
    ) -> Result<Value, std::io::Error> {
        let props = properties.join(",");
        let path = format!("/crm/v3/objects/deals/{deal_id}?propertiesWithHistory={props}");
        self.get_json(&path, "deal_history.json").await
    }

    pub async fn contact_with_property_history(
        &self,
        contact_id: &str,
        properties: &[&str],
    ) -> Result<Value, std::io::Error> {
        let props = properties.join(",");
        let path = format!("/crm/v3/objects/contacts/{contact_id}?propertiesWithHistory={props}");
        self.get_json(&path, "contact_history.json").await
    }

    pub async fn list_marketing_emails(&self) -> Result<Value, std::io::Error> {
        self.get_json("/marketing/v3/emails?limit=50", "marketing_emails.json")
            .await
    }
}

pub fn prop_str(props: &Value, key: &str) -> Option<String> {
    props.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_loads_deals() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body = HubspotApiClient::fixture_path(dir, "deals.json").expect("deals fixture");
        assert!(body.get("results").is_some());
    }
}
