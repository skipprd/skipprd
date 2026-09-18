use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{DateWindowPlanner, RetryConfig, RetryableHttpClient};
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

use crate::shopify_api::{
    api_version_or_default, gid_tail, money_amount, normalize_shop_domain, strip_pii_value,
    ShopifyGraphqlClient, FIXTURE_ENV,
};
use crate::streams::{
    resolve_streams, ShopifyStream, StreamProfile, NAMESPACE_ORDER_FACT, NAMESPACE_ORDER_LINE_FACT,
    NAMESPACE_PRODUCT_SNAPSHOT, NAMESPACE_STORE_SNAPSHOT, NAMESPACE_SYNC_RUN_DAILY,
};

pub const NAMESPACE_REFUND_FACT: &str = "shopify_refund_fact";
pub const NAMESPACE_VARIANT_SNAPSHOT: &str = "shopify_variant_snapshot";
pub const NAMESPACE_COLLECTION_SNAPSHOT: &str = "shopify_collection_snapshot";
pub const NAMESPACE_CONTENT_PAGE_SNAPSHOT: &str = "shopify_content_page_snapshot";
pub const NAMESPACE_REDIRECT_SNAPSHOT: &str = "shopify_redirect_snapshot";
pub const NAMESPACE_DISCOUNT_SNAPSHOT: &str = "shopify_discount_snapshot";
pub const NAMESPACE_MARKETING_EVENT_FACT: &str = "shopify_marketing_event_fact";

const DISCOVER_SAMPLE_DAYS: u32 = 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ShopifyOrderCheckpoint {
    last_completed_date: String,
}

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Deserialize, SkipprConfig)]
pub struct DataSourceShopifyAdminPluginConfig {
    pub shop_domain: String,
    #[serde(default)]
    pub api_version: Option<String>,
    pub start_date: String,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default)]
    pub stream_profile: StreamProfile,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    #[serde(default = "default_min_query_interval_ms")]
    pub min_query_interval_ms: u64,
    #[serde(default = "default_max_queries_per_run")]
    pub max_queries_per_run: u32,
    #[serde(default)]
    pub use_bulk_operations: bool,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    #[skippr(secret)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    #[skippr(secret)]
    pub oauth_access_token: Option<String>,
}

fn default_lookback_days() -> u32 {
    7
}

fn default_min_query_interval_ms() -> u64 {
    500
}

fn default_max_queries_per_run() -> u32 {
    200
}

impl DataSourceShopifyAdminPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if normalize_shop_domain(&self.shop_domain).is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "shop_domain is required",
            ));
        }
        NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        Ok(())
    }
}

pub struct DataSourceShopifyAdminPlugin {
    config: DataSourceShopifyAdminPluginConfig,
    shop_domain: String,
    access_token: String,
}

impl DataSourceShopifyAdminPlugin {
    pub fn new(config: DataSourceShopifyAdminPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let access_token = Self::resolve_access_token(&config)?;
        Ok(Self {
            shop_domain: normalize_shop_domain(&config.shop_domain),
            access_token,
            config,
        })
    }

    fn resolve_access_token(
        config: &DataSourceShopifyAdminPluginConfig,
    ) -> Result<String, std::io::Error> {
        if let Some(token) = config
            .oauth_access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(token.trim().to_string());
        }
        if std::env::var(FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok("fixture".into());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Shopify Admin requires oauth_access_token (or SKIPPR_SHOPIFY_FIXTURE_DIR for tests)",
        ))
    }

    fn graphql_client(&self) -> ShopifyGraphqlClient {
        let http = RetryableHttpClient::new(RetryConfig::default());
        ShopifyGraphqlClient::new(
            http,
            &self.shop_domain,
            &api_version_or_default(self.config.api_version.as_deref()),
            self.access_token.clone(),
            self.config.min_query_interval_ms,
            self.config.max_queries_per_run,
        )
    }

    fn selected_streams(&self) -> Vec<ShopifyStream> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<ShopifyStream> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(namespace: &str, lookback_days: u32) -> SourceNamespaceContract {
        match namespace {
            NAMESPACE_STORE_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("shop_domain"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Shopify shop settings snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_PRODUCT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("product_id"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Shopify product catalog snapshot".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_ORDER_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("order_id")],
                cursor: Some(FieldPath::single("order_date")),
                partition_key: vec![FieldPath::single("order_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: Some(lookback_days),
                description: "Shopify orders without PII".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_ORDER_LINE_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("order_id"), FieldPath::single("line_id")],
                cursor: Some(FieldPath::single("order_date")),
                partition_key: vec![FieldPath::single("order_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: Some(lookback_days),
                description: "Shopify order line items".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_REFUND_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("order_id"),
                    FieldPath::single("refund_id"),
                ],
                cursor: Some(FieldPath::single("order_date")),
                partition_key: vec![FieldPath::single("order_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: Some(lookback_days),
                description: "Shopify refunds without PII".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_VARIANT_SNAPSHOT
            | NAMESPACE_COLLECTION_SNAPSHOT
            | NAMESPACE_CONTENT_PAGE_SNAPSHOT
            | NAMESPACE_REDIRECT_SNAPSHOT
            | NAMESPACE_DISCOUNT_SNAPSHOT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("id"), FieldPath::single("run_date")],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: format!("Shopify snapshot: {namespace}"),
                semantics: Some(SourceSemantics::MutableReport),
            },
            NAMESPACE_MARKETING_EVENT_FACT => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![FieldPath::single("event_id")],
                cursor: Some(FieldPath::single("occurred_at")),
                partition_key: vec![FieldPath::single("occurred_at")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: Some(lookback_days),
                description: "Shopify marketing events".into(),
                semantics: Some(SourceSemantics::EventStream),
            },
            NAMESPACE_SYNC_RUN_DAILY => SourceNamespaceContract {
                namespace: namespace.to_string(),
                primary_key: vec![
                    FieldPath::single("shop_domain"),
                    FieldPath::single("run_date"),
                ],
                cursor: Some(FieldPath::single("run_date")),
                partition_key: vec![FieldPath::single("run_date")],
                write_policy: WritePolicy::ReplacePartition,
                refresh_window: None,
                description: "Shopify connector sync health".into(),
                semantics: Some(SourceSemantics::MutableReport),
            },
            other => panic!("unknown shopify namespace: {other}"),
        }
    }

    fn submit_namespace(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
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
        let offset_key = OffsetKey::new(namespace, run_date.to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("shopify://{}/{}", self.shop_domain, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn order_checkpoint_key(&self) -> String {
        format!("shopify:{}:{}", self.shop_domain, NAMESPACE_ORDER_FACT)
    }

    fn load_last_completed_order_date(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        load_checkpoint_payload::<ShopifyOrderCheckpoint>(ctx, key)
            .and_then(|cp| NaiveDate::parse_from_str(&cp.last_completed_date, "%Y-%m-%d").ok())
    }

    fn store_last_completed_order_date(
        ctx: &dyn SourceSyncContext,
        key: &str,
        date: NaiveDate,
    ) -> Result<(), std::io::Error> {
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &ShopifyOrderCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    fn order_date_from_created_at(created_at: Option<&str>, fallback: &str) -> String {
        created_at
            .and_then(|s| s.get(0..10))
            .unwrap_or(fallback)
            .to_string()
    }

    fn submit_rows_by_partition_field(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        field: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut by_date: HashMap<String, Vec<Value>> = HashMap::new();
        for row in rows {
            let date = row
                .get(field)
                .and_then(|v| v.as_str())
                .and_then(|s| s.get(0..10))
                .unwrap_or("")
                .to_string();
            if date.is_empty() {
                continue;
            }
            by_date.entry(date).or_default().push(row);
        }
        for (date, batch) in by_date {
            self.submit_namespace(ctx, namespace, &date, batch)?;
        }
        Ok(())
    }

    fn submit_rows_by_order_date(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        self.submit_rows_by_partition_field(ctx, namespace, "order_date", rows)
    }

    fn order_date_window(
        &self,
        discover: bool,
        last_completed: Option<NaiveDate>,
    ) -> Result<(NaiveDate, NaiveDate), std::io::Error> {
        let end = Utc::now().date_naive() - Duration::days(1);
        let configured_start = NaiveDate::parse_from_str(&self.config.start_date, "%Y-%m-%d")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let start = if discover {
            end - Duration::days(i64::from(DISCOVER_SAMPLE_DAYS.saturating_sub(1)))
        } else {
            let planner = DateWindowPlanner {
                lookback_days: self.config.lookback_days,
            };
            let window = planner.plan(configured_start, last_completed, end);
            window.start
        };
        if end < start {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Shopify order window end {end} is before start {start}"),
            ));
        }
        Ok((start, end))
    }

    async fn sync_store(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut body = client.fetch_shop().await?;
        strip_pii_value(&mut body);
        let shop = body
            .pointer("/data/shop")
            .ok_or_else(|| std::io::Error::other("Shopify shop query missing data.shop"))?;
        Ok(vec![json!({
            "run_date": run_date,
            "shop_domain": self.shop_domain,
            "shop_name": shop.get("name").and_then(|v| v.as_str()),
            "currency_code": shop.get("currencyCode").and_then(|v| v.as_str()),
            "plan_display_name": shop.pointer("/plan/displayName").and_then(|v| v.as_str()),
            "primary_domain_url": shop.pointer("/primaryDomain/url").and_then(|v| v.as_str()),
        })])
    }

    async fn sync_catalog(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client.fetch_products_page(cursor.as_deref()).await?;
            strip_pii_value(&mut body);
            let products = body
                .pointer("/data/products")
                .ok_or_else(|| std::io::Error::other("missing data.products"))?;
            if let Some(nodes) = products.get("nodes").and_then(|v| v.as_array()) {
                for node in nodes {
                    rows.push(json!({
                        "run_date": run_date,
                        "product_id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "handle": node.get("handle").and_then(|v| v.as_str()),
                        "title": node.get("title").and_then(|v| v.as_str()),
                        "status": node.get("status").and_then(|v| v.as_str()),
                        "product_type": node.get("productType").and_then(|v| v.as_str()),
                        "vendor": node.get("vendor").and_then(|v| v.as_str()),
                        "online_store_url": node.get("onlineStoreUrl").and_then(|v| v.as_str()),
                        "seo_title": node.pointer("/seo/title").and_then(|v| v.as_str()),
                        "seo_description": node.pointer("/seo/description").and_then(|v| v.as_str()),
                        "published_at": node.get("publishedAt").and_then(|v| v.as_str()),
                        "updated_at": node.get("updatedAt").and_then(|v| v.as_str()),
                        "variant_count": node.pointer("/variantsCount/count").and_then(|v| v.as_i64()),
                        "total_inventory": node.get("totalInventory").and_then(|v| v.as_i64()),
                    }));
                }
            }
            let page_info = products.get("pageInfo");
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    fn parse_order_nodes(
        &self,
        nodes: &[Value],
        ingest_run_date: &str,
    ) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let mut order_rows = Vec::new();
        let mut line_rows = Vec::new();
        let mut refund_rows = Vec::new();
        for node in nodes {
            let order_id = node
                .get("id")
                .and_then(|v| v.as_str())
                .map(gid_tail)
                .unwrap_or_default();
            let created_at = node.get("createdAt").and_then(|v| v.as_str());
            let order_date = Self::order_date_from_created_at(created_at, ingest_run_date);
            let journey = node.get("customerJourneySummary");
            let first_visit = journey.and_then(|j| j.get("firstVisit"));
            let utm = first_visit.and_then(|v| v.get("utmParameters"));
            let discount_codes = node
                .get("discountCodes")
                .and_then(|v| v.as_array())
                .map(|codes| {
                    codes
                        .iter()
                        .filter_map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|s| !s.is_empty());
            order_rows.push(json!({
                "ingest_run_date": ingest_run_date,
                "order_date": order_date,
                "order_id": order_id,
                "created_at": created_at,
                "financial_status": node.get("displayFinancialStatus").and_then(|v| v.as_str()),
                "fulfillment_status": node.get("displayFulfillmentStatus").and_then(|v| v.as_str()),
                "currency_code": node.get("currencyCode").and_then(|v| v.as_str()),
                "total_price": money_amount(node.get("totalPriceSet")),
                "subtotal_price": money_amount(node.get("subtotalPriceSet")),
                "total_tax": money_amount(node.get("totalTaxSet")),
                "total_shipping": money_amount(node.get("totalShippingPriceSet")),
                "total_discounts": money_amount(node.get("totalDiscountsSet")),
                "landing_page_url": first_visit.and_then(|v| v.get("landingPage")).and_then(|v| v.as_str()),
                "referring_site": first_visit.and_then(|v| v.get("referrerUrl")).and_then(|v| v.as_str()),
                "utm_source": utm.and_then(|u| u.get("source")).and_then(|v| v.as_str()),
                "utm_medium": utm.and_then(|u| u.get("medium")).and_then(|v| v.as_str()),
                "utm_campaign": utm.and_then(|u| u.get("campaign")).and_then(|v| v.as_str()),
                "sales_channel": node.pointer("/channelInformation/channelDefinition/channelName").and_then(|v| v.as_str()),
                "discount_codes": discount_codes,
            }));
            if let Some(lines) = node.pointer("/lineItems/nodes").and_then(|v| v.as_array()) {
                for line in lines {
                    line_rows.push(json!({
                        "ingest_run_date": ingest_run_date,
                        "order_date": order_date,
                        "order_id": order_id,
                        "line_id": line.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "product_id": line.pointer("/product/id").and_then(|v| v.as_str()).map(gid_tail),
                        "variant_id": line.pointer("/variant/id").and_then(|v| v.as_str()).map(gid_tail),
                        "sku": line.get("sku").and_then(|v| v.as_str()),
                        "title": line.get("title").and_then(|v| v.as_str()),
                        "quantity": line.get("quantity").and_then(|v| v.as_i64()),
                        "line_total": money_amount(line.get("originalTotalSet")),
                    }));
                }
            }
            if let Some(refunds) = node.pointer("/refunds").and_then(|v| v.as_array()) {
                for refund in refunds {
                    let refund_id = refund.get("id").and_then(|v| v.as_str()).map(gid_tail);
                    refund_rows.push(json!({
                        "ingest_run_date": ingest_run_date,
                        "order_date": order_date,
                        "order_id": order_id,
                        "refund_id": refund_id,
                        "created_at": refund.get("createdAt").and_then(|v| v.as_str()),
                    }));
                }
            }
        }
        (order_rows, line_rows, refund_rows)
    }

    async fn fetch_orders_for_day(
        &self,
        client: &ShopifyGraphqlClient,
        day: NaiveDate,
    ) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>), std::io::Error> {
        let day_str = day.format("%Y-%m-%d").to_string();
        let next_day = (day + Duration::days(1)).format("%Y-%m-%d").to_string();
        let search_query = format!("created_at:>={day_str} created_at:<{next_day}");
        let mut order_rows = Vec::new();
        let mut line_rows = Vec::new();
        let mut refund_rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client
                .fetch_orders_page(cursor.as_deref(), &search_query)
                .await?;
            strip_pii_value(&mut body);
            let orders = body
                .pointer("/data/orders")
                .ok_or_else(|| std::io::Error::other("missing data.orders"))?;
            if let Some(nodes) = orders.get("nodes").and_then(|v| v.as_array()) {
                let (o, l, r) = self.parse_order_nodes(nodes, &day_str);
                order_rows.extend(o);
                line_rows.extend(l);
                refund_rows.extend(r);
            }
            let page_info = orders.get("pageInfo");
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok((order_rows, line_rows, refund_rows))
    }

    async fn sync_orders(
        &self,
        ctx: &dyn SourceSyncContext,
        client: &ShopifyGraphqlClient,
        _ingest_run_date: &str,
        discover: bool,
    ) -> Result<(), std::io::Error> {
        let checkpoint_key = self.order_checkpoint_key();
        let last_completed = if discover {
            None
        } else {
            Self::load_last_completed_order_date(ctx, &checkpoint_key)
        };
        let (start, end) = self.order_date_window(discover, last_completed)?;
        let mut cursor_day = start;
        while cursor_day <= end {
            let (orders, lines, refunds) = self.fetch_orders_for_day(client, cursor_day).await?;
            self.submit_rows_by_order_date(ctx, NAMESPACE_ORDER_FACT, orders)?;
            self.submit_rows_by_order_date(ctx, NAMESPACE_ORDER_LINE_FACT, lines)?;
            self.submit_rows_by_order_date(ctx, NAMESPACE_REFUND_FACT, refunds)?;
            if !discover {
                Self::store_last_completed_order_date(ctx, &checkpoint_key, cursor_day)?;
            }
            cursor_day += Duration::days(1);
        }
        Ok(())
    }

    async fn sync_content(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), std::io::Error> {
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client.fetch_pages_page(cursor.as_deref()).await?;
            strip_pii_value(&mut body);
            let nodes = body.pointer("/data/pages/nodes").and_then(|v| v.as_array());
            if let Some(nodes) = nodes {
                for node in nodes {
                    pages.push(json!({
                        "run_date": run_date,
                        "id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "title": node.get("title").and_then(|v| v.as_str()),
                        "handle": node.get("handle").and_then(|v| v.as_str()),
                        "is_published": node.get("isPublished").and_then(|v| v.as_bool()),
                        "updated_at": node.get("updatedAt").and_then(|v| v.as_str()),
                    }));
                }
            }
            let page_info = body.pointer("/data/pages/pageInfo");
            if !page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        let mut redirects = Vec::new();
        cursor = None;
        loop {
            let mut body = client.fetch_redirects_page(cursor.as_deref()).await?;
            strip_pii_value(&mut body);
            if let Some(nodes) = body
                .pointer("/data/urlRedirects/nodes")
                .and_then(|v| v.as_array())
            {
                for node in nodes {
                    redirects.push(json!({
                        "run_date": run_date,
                        "id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "path": node.get("path").and_then(|v| v.as_str()),
                        "target": node.get("target").and_then(|v| v.as_str()),
                    }));
                }
            }
            let page_info = body.pointer("/data/urlRedirects/pageInfo");
            if !page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok((pages, redirects))
    }

    async fn sync_collections(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client.fetch_collections_page(cursor.as_deref()).await?;
            strip_pii_value(&mut body);
            if let Some(nodes) = body
                .pointer("/data/collections/nodes")
                .and_then(|v| v.as_array())
            {
                for node in nodes {
                    rows.push(json!({
                        "run_date": run_date,
                        "id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "title": node.get("title").and_then(|v| v.as_str()),
                        "handle": node.get("handle").and_then(|v| v.as_str()),
                        "updated_at": node.get("updatedAt").and_then(|v| v.as_str()),
                    }));
                }
            }
            let page_info = body.pointer("/data/collections/pageInfo");
            if !page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    async fn sync_discounts(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client.fetch_discounts_page(cursor.as_deref()).await?;
            strip_pii_value(&mut body);
            if let Some(nodes) = body
                .pointer("/data/codeDiscountNodes/nodes")
                .and_then(|v| v.as_array())
            {
                for node in nodes {
                    let discount = node.get("codeDiscount");
                    rows.push(json!({
                        "run_date": run_date,
                        "id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "title": discount.and_then(|d| d.get("title")).and_then(|v| v.as_str()),
                        "status": discount.and_then(|d| d.get("status")).and_then(|v| v.as_str()),
                        "starts_at": discount.and_then(|d| d.get("startsAt")).and_then(|v| v.as_str()),
                        "ends_at": discount.and_then(|d| d.get("endsAt")).and_then(|v| v.as_str()),
                    }));
                }
            }
            let page_info = body.pointer("/data/codeDiscountNodes/pageInfo");
            if !page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    async fn sync_marketing(
        &self,
        client: &ShopifyGraphqlClient,
        run_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = client
                .fetch_marketing_events_page(cursor.as_deref())
                .await?;
            strip_pii_value(&mut body);
            if let Some(nodes) = body
                .pointer("/data/marketingEvents/nodes")
                .and_then(|v| v.as_array())
            {
                for node in nodes {
                    let occurred = node
                        .get("startedAt")
                        .and_then(|v| v.as_str())
                        .map(|s| s.get(0..10).unwrap_or(run_date).to_string())
                        .unwrap_or_else(|| run_date.to_string());
                    rows.push(json!({
                        "event_id": node.get("id").and_then(|v| v.as_str()).map(gid_tail),
                        "occurred_at": occurred,
                        "event_type": node.get("type").and_then(|v| v.as_str()),
                        "utm_campaign": node.get("utmCampaign").and_then(|v| v.as_str()),
                        "utm_source": node.get("utmSource").and_then(|v| v.as_str()),
                        "utm_medium": node.get("utmMedium").and_then(|v| v.as_str()),
                        "ended_at": node.get("endedAt").and_then(|v| v.as_str()),
                    }));
                }
            }
            let page_info = body.pointer("/data/marketingEvents/pageInfo");
            if !page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                break;
            }
            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(rows)
    }
}

#[async_trait]
impl DataSource for DataSourceShopifyAdminPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let namespaces = [
            NAMESPACE_STORE_SNAPSHOT,
            NAMESPACE_PRODUCT_SNAPSHOT,
            NAMESPACE_VARIANT_SNAPSHOT,
            NAMESPACE_ORDER_FACT,
            NAMESPACE_ORDER_LINE_FACT,
            NAMESPACE_REFUND_FACT,
            NAMESPACE_COLLECTION_SNAPSHOT,
            NAMESPACE_CONTENT_PAGE_SNAPSHOT,
            NAMESPACE_REDIRECT_SNAPSHOT,
            NAMESPACE_DISCOUNT_SNAPSHOT,
            NAMESPACE_MARKETING_EVENT_FACT,
            NAMESPACE_SYNC_RUN_DAILY,
        ];
        namespaces
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
        let client = self.graphql_client();
        let api_errors: u32 = 0;
        let mut synced_streams: Vec<&str> = Vec::new();
        let streams = self.streams_for_run(discover);

        if self.config.use_bulk_operations {
            info!("Shopify Admin: use_bulk_operations is set; minimal plugin uses GraphQL pagination only");
        }

        for stream in &streams {
            let result: Result<(), std::io::Error> = match stream {
                ShopifyStream::Store => {
                    let rows = self.sync_store(&client, &run_date).await?;
                    self.submit_namespace(ctx.as_ref(), NAMESPACE_STORE_SNAPSHOT, &run_date, rows)?;
                    Ok(())
                }
                ShopifyStream::Catalog => {
                    let rows = self.sync_catalog(&client, &run_date).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_PRODUCT_SNAPSHOT,
                        &run_date,
                        rows,
                    )?;
                    Ok(())
                }
                ShopifyStream::Orders => {
                    self.sync_orders(ctx.as_ref(), &client, &run_date, discover)
                        .await?;
                    Ok(())
                }
                ShopifyStream::Content => {
                    let (pages, redirects) = self.sync_content(&client, &run_date).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_CONTENT_PAGE_SNAPSHOT,
                        &run_date,
                        pages,
                    )?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_REDIRECT_SNAPSHOT,
                        &run_date,
                        redirects,
                    )?;
                    Ok(())
                }
                ShopifyStream::Collections => {
                    let rows = self.sync_collections(&client, &run_date).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_COLLECTION_SNAPSHOT,
                        &run_date,
                        rows,
                    )?;
                    Ok(())
                }
                ShopifyStream::Discounts => {
                    let rows = self.sync_discounts(&client, &run_date).await?;
                    self.submit_namespace(
                        ctx.as_ref(),
                        NAMESPACE_DISCOUNT_SNAPSHOT,
                        &run_date,
                        rows,
                    )?;
                    Ok(())
                }
                ShopifyStream::Marketing => {
                    let rows = self.sync_marketing(&client, &run_date).await?;
                    self.submit_rows_by_partition_field(
                        ctx.as_ref(),
                        NAMESPACE_MARKETING_EVENT_FACT,
                        "occurred_at",
                        rows,
                    )?;
                    Ok(())
                }
            };
            match result {
                Ok(()) => synced_streams.push(stream.name()),
                Err(err) => {
                    warn!(stream = stream.name(), error = %err, "Shopify Admin stream sync failed");
                    return Err(err);
                }
            }
        }

        let status = if api_errors == 0 { "ok" } else { "error" };
        let sync_row = json!({
            "run_date": run_date,
            "shop_domain": self.shop_domain,
            "status": status,
            "streams_synced": synced_streams.join(","),
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
            shop_domain = %self.shop_domain,
            streams = %synced_streams.join(","),
            queries = client.queries_used(),
            elapsed_ms = started.elapsed().as_millis(),
            "Shopify Admin sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };
    use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn config_deserializes_console_default_profile() {
        let cfg: DataSourceShopifyAdminPluginConfig = serde_json::from_value(json!({
            "shop_domain": "example.myshopify.com",
            "start_date": "2024-01-01",
            "stream_profile": "console_default",
            "oauth_access_token": "shpat_test"
        }))
        .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::ConsoleDefault);
        cfg.validate().expect("valid");
    }

    #[test]
    fn namespace_contracts_validate() {
        let mut cfg = sample_config();
        cfg.oauth_access_token = Some("token".into());
        let plugin = DataSourceShopifyAdminPlugin::new(cfg).unwrap();
        for contract in plugin.source_namespace_contracts() {
            contract.validate().expect("valid contract");
        }
    }

    #[test]
    fn missing_token_without_fixture_fails() {
        let _lock = env_lock();
        clear_fixture_dir();
        let err = match DataSourceShopifyAdminPlugin::new(sample_config()) {
            Err(err) => err,
            Ok(_) => panic!("expected missing token error"),
        };
        assert!(err.to_string().contains("oauth_access_token"));
    }

    #[tokio::test]
    async fn happy_sync() {
        let _lock = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataSourceShopifyAdminPlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let store = ctx.rows_for_namespace(NAMESPACE_STORE_SNAPSHOT);
        assert_eq!(store.len(), 1);
        assert_eq!(store[0]["shop_name"], "Fixture Shop");

        let products = ctx.rows_for_namespace(NAMESPACE_PRODUCT_SNAPSHOT);
        assert_eq!(products.len(), 1);
        assert_eq!(products[0]["handle"], "fixture-product");

        let orders = ctx.rows_for_namespace(NAMESPACE_ORDER_FACT);
        assert!(!orders.is_empty());
        assert!(orders
            .iter()
            .any(|o| o.get("order_id").and_then(|v| v.as_str()) == Some("5001")));
        assert!(orders
            .iter()
            .all(|o| o.get("order_date").and_then(|v| v.as_str()).is_some()));
        assert!(orders[0].get("email").is_none());

        let lines = ctx.rows_for_namespace(NAMESPACE_ORDER_LINE_FACT);
        assert!(!lines.is_empty());
        assert!(lines
            .iter()
            .any(|l| l.get("sku").and_then(|v| v.as_str()) == Some("FIX-SKU-1")));

        let runs = ctx.rows_for_namespace(NAMESPACE_SYNC_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");
        assert!(runs[0]["streams_synced"]
            .as_str()
            .unwrap_or("")
            .contains("store"));

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn discover_sync_emits_minimal_streams_only() {
        let _lock = env_lock();
        set_fixture_dir();
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut plugin = DataSourceShopifyAdminPlugin::new(sample_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("discover sync");

        assert!(!ctx.rows_for_namespace(NAMESPACE_STORE_SNAPSHOT).is_empty());
        assert!(!ctx.rows_for_namespace(NAMESPACE_SYNC_RUN_DAILY).is_empty());

        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
        clear_fixture_dir();
    }
}
