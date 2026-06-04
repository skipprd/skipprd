use serde_json::{json, Value};
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tokio::time::{sleep, Duration};
pub const FIXTURE_ENV: &str = "SKIPPR_SHOPIFY_FIXTURE_DIR";

const SHOP_QUERY: &str = r#"query ShopSnapshot {
  shop {
    name
    currencyCode
    primaryDomain { url }
    plan { displayName }
  }
}"#;

const PRODUCTS_QUERY: &str = r#"query ProductCatalog($cursor: String) {
  products(first: 50, after: $cursor) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      handle
      title
      status
      productType
      vendor
      onlineStoreUrl
      seo { title description }
      publishedAt
      updatedAt
      totalInventory
      variantsCount { count }
    }
  }
}"#;

const ORDERS_QUERY: &str = r#"query OrdersInRange($cursor: String, $query: String) {
  orders(first: 50, after: $cursor, query: $query, sortKey: CREATED_AT, reverse: true) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      createdAt
      displayFinancialStatus
      displayFulfillmentStatus
      currencyCode
      totalPriceSet { shopMoney { amount } }
      subtotalPriceSet { shopMoney { amount } }
      totalTaxSet { shopMoney { amount } }
      totalShippingPriceSet { shopMoney { amount } }
      totalDiscountsSet { shopMoney { amount } }
      customerJourneySummary {
        firstVisit {
          landingPage
          referrerUrl
          utmParameters { source medium campaign }
        }
      }
      channelInformation { channelDefinition { channelName } }
      discountCodes
      lineItems(first: 100) {
        nodes {
          id
          sku
          title
          quantity
          originalTotalSet { shopMoney { amount } }
          product { id }
          variant { id }
        }
      }
    }
  }
}"#;

#[derive(Clone)]
pub struct ShopifyGraphqlClient {
    pub http: RetryableHttpClient,
    pub endpoint: String,
    pub access_token: String,
    pub min_query_interval_ms: u64,
    pub max_queries_per_run: u32,
    query_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl ShopifyGraphqlClient {
    pub fn new(
        http: RetryableHttpClient,
        shop_domain: &str,
        api_version: &str,
        access_token: String,
        min_query_interval_ms: u64,
        max_queries_per_run: u32,
    ) -> Self {
        let domain = normalize_shop_domain(shop_domain);
        let version = api_version.trim().trim_matches('/');
        let endpoint = format!("https://{domain}/admin/api/{version}/graphql.json");
        Self {
            http,
            endpoint,
            access_token,
            min_query_interval_ms,
            max_queries_per_run,
            query_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    pub fn queries_used(&self) -> u32 {
        self.query_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn fetch_shop(&self) -> Result<Value, std::io::Error> {
        self.execute("shop", SHOP_QUERY, json!({})).await
    }

    pub async fn fetch_products_page(&self, cursor: Option<&str>) -> Result<Value, std::io::Error> {
        let variables = match cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({ "cursor": null }),
        };
        self.execute("products", PRODUCTS_QUERY, variables).await
    }

    pub async fn fetch_orders_page(
        &self,
        cursor: Option<&str>,
        search_query: &str,
    ) -> Result<Value, std::io::Error> {
        let variables = json!({
            "cursor": cursor,
            "query": search_query,
        });
        self.execute("orders", ORDERS_QUERY, variables).await
    }

    async fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
    ) -> Result<Value, std::io::Error> {
        if let Ok(dir) = std::env::var(FIXTURE_ENV) {
            if let Some(body) = load_fixture_response(&dir, operation) {
                return Ok(body);
            }
        }

        let used = self.query_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if used >= self.max_queries_per_run {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Shopify Admin max_queries_per_run ({}) exceeded",
                    self.max_queries_per_run
                ),
            ));
        }

        if used > 0 && self.min_query_interval_ms > 0 {
            sleep(Duration::from_millis(self.min_query_interval_ms)).await;
        }

        let body = json!({ "query": query, "variables": variables });
        let response = self
            .http
            .client
            .post(&self.endpoint)
            .header("X-Shopify-Access-Token", &self.access_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(std::io::Error::other)?;

        let status = response.status();
        let text = response.text().await.map_err(std::io::Error::other)?;
        if !status.is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Shopify GraphQL HTTP {status}: {text}"),
            ));
        }

        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(errors) = parsed.get("errors").and_then(|v| v.as_array()) {
            if !errors.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Shopify GraphQL errors: {errors:?}"),
                ));
            }
        }
        Ok(parsed)
    }
}

pub fn normalize_shop_domain(shop_domain: &str) -> String {
    let mut domain = shop_domain.trim().to_string();
    domain = domain
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    domain
}

pub fn api_version_or_default(api_version: Option<&str>) -> String {
    api_version
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().trim_matches('/').to_string())
        .unwrap_or_else(|| "2026-04".to_string())
}

pub fn gid_tail(gid: &str) -> String {
    gid.rsplit('/').next().unwrap_or(gid).to_string()
}

pub fn money_amount(set: Option<&Value>) -> Option<String> {
    set.and_then(|v| v.get("shopMoney"))
        .and_then(|m| m.get("amount"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
}

fn load_fixture_response(dir: &str, operation: &str) -> Option<Value> {
    let file = match operation {
        "shop" => "shop.json",
        "products" => "products.json",
        "orders" => "orders.json",
        _ => return None,
    };
    let path = format!("{}/{}", dir.trim_end_matches('/'), file);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove known PII keys if they appear in nested JSON (defense in depth).
pub fn strip_pii_value(value: &mut Value) {
    const DENY: &[&str] = &[
        "email",
        "phone",
        "firstName",
        "lastName",
        "address1",
        "address2",
        "city",
        "province",
        "zip",
        "country",
        "note",
        "paymentDetails",
    ];
    match value {
        Value::Object(map) => {
            for key in DENY {
                map.remove(*key);
            }
            for v in map.values_mut() {
                strip_pii_value(v);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_pii_value(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_shop_domain_strips_scheme() {
        assert_eq!(
            normalize_shop_domain("https://example.myshopify.com/"),
            "example.myshopify.com"
        );
    }

    #[test]
    fn gid_tail_extracts_numeric_id() {
        assert_eq!(
            gid_tail("gid://shopify/Product/12345"),
            "12345"
        );
    }

    #[test]
    fn strip_pii_removes_email_from_object() {
        let mut v = json!({"email": "a@b.com", "id": "1"});
        strip_pii_value(&mut v);
        assert!(v.get("email").is_none());
        assert_eq!(v["id"], "1");
    }

    #[test]
    fn load_fixture_reads_shop_file() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body = load_fixture_response(dir, "shop").expect("shop fixture");
        assert!(body.pointer("/data/shop/name").is_some());
    }
}
