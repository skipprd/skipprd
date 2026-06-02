use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Public `skippr` config schema.
///
/// This is the only config surface exposed to product users.
/// It maps to the internal runtime config shape silently.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedPromptEntry {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprProjectConfig {
    pub project: String,

    #[serde(default)]
    pub warehouse: Option<WarehouseConfig>,

    #[serde(default)]
    pub source: Option<SourceConfig>,

    #[serde(default)]
    pub dbt: Option<DbtConfig>,

    #[serde(default)]
    pub schema_sink: Option<SchemaSinkConfig>,

    /// Named doc trees for `skippr vector ingest-docs` (declarative include/exclude; no CLI defaults).
    #[serde(default)]
    pub vector_sources: std::collections::HashMap<String, VectorSourceEntry>,

    /// Pipeline entries from `skippr.yml` (ELT pipelines and vector-ingest blocks under `pipelines:`).
    #[serde(default)]
    pub pipelines: std::collections::HashMap<String, serde_yaml::Value>,
}

/// Parsed `pipelines.<name>` block for `skippr vector ingest-docs` (expects `vector_source`).
#[derive(Clone, Debug)]
pub struct VectorIngestPipelineSpec {
    /// Key under `vector_sources` to ingest.
    pub vector_source: String,
    pub chunk_chars: Option<usize>,
    pub chunk_overlap: Option<usize>,
}

fn yaml_mapping_get_str<'a>(m: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    m.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn yaml_mapping_get_usize(m: &serde_yaml::Mapping, key: &str) -> Option<usize> {
    m.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64().map(|n| n as usize))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VectorSourceEntry {
    /// Directory root for discovery (relative paths resolve against the `skippr.yml` directory).
    pub root: String,
    /// Glob patterns relative to `root` (e.g. `**/*.md`).
    #[serde(default)]
    pub include: Vec<String>,
    /// Glob patterns relative to `root` excluded after include.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// If set, only files whose extension (lowercase, no dot) is listed are kept.
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    /// Optional per-source chunk size (characters).
    #[serde(default)]
    pub chunk_chars: Option<usize>,
    /// Optional per-source chunk overlap (characters).
    #[serde(default)]
    pub chunk_overlap: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WarehouseConfig {
    Athena {
        #[serde(default)]
        workgroup: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        result_s3: Option<String>,
        #[serde(default)]
        schema: Option<String>,
    },
    Snowflake {
        #[serde(default)]
        account: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        private_key_path: Option<String>,
        #[serde(default)]
        stage: Option<String>,
        #[serde(default)]
        staging_uri: Option<String>,
        #[serde(default)]
        staging_storage_integration: Option<String>,
        #[serde(default)]
        staging_azure_sas_token: Option<String>,
        #[serde(default)]
        staging_azure_account_key: Option<String>,
        #[serde(default)]
        staging_gcs_service_account_key_path: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        schema: Option<String>,
        #[serde(default)]
        warehouse: Option<String>,
        #[serde(default)]
        role: Option<String>,
    },
    Bigquery {
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        dataset: Option<String>,
        #[serde(default)]
        location: Option<String>,
    },
    Postgres {
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        schema: Option<String>,
    },
    Databricks {
        #[serde(default)]
        workspace_url: Option<String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        warehouse_id: Option<String>,
        #[serde(default)]
        catalog: Option<String>,
        #[serde(default)]
        schema: Option<String>,
    },
    Synapse {
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        schema: Option<String>,
    },
    Redshift {
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        cluster_identifier: Option<String>,
        #[serde(default)]
        workgroup_name: Option<String>,
        #[serde(default)]
        db_user: Option<String>,
        #[serde(default)]
        schema: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        staging_s3_bucket: Option<String>,
        #[serde(default)]
        staging_s3_prefix: Option<String>,
        #[serde(default)]
        iam_role_arn: Option<String>,
    },
    Clickhouse {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Motherduck {
        #[serde(default)]
        motherduck_token: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        schema: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoogleSerpTargetConfig {
    pub site: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    Mssql {
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
    },
    S3 {
        #[serde(default)]
        s3_bucket: Option<String>,
        #[serde(default)]
        s3_prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transform: Option<S3Transform>,
    },
    Mysql {
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
    },
    #[serde(rename = "postgres_source")]
    PostgresSource {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
        #[serde(default)]
        query: Option<String>,
    },
    #[serde(rename = "redshift_source")]
    RedshiftSource {
        #[serde(default)]
        cluster_identifier: Option<String>,
        #[serde(default)]
        workgroup_name: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        db_user: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
        #[serde(default)]
        region: Option<String>,
    },
    Mongodb {
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        filter: Option<String>,
    },
    Dynamodb {
        #[serde(default)]
        table_name: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        endpoint_url: Option<String>,
    },
    #[serde(rename = "clickhouse_source")]
    ClickhouseSource {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
        #[serde(default)]
        query: Option<String>,
    },
    #[serde(rename = "motherduck_source")]
    MotherduckSource {
        #[serde(default)]
        motherduck_token: Option<String>,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        tables: Option<Vec<String>>,
        #[serde(default)]
        query: Option<String>,
    },
    Sftp {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        private_key_path: Option<String>,
        #[serde(default)]
        remote_path: Option<String>,
    },
    File {
        #[serde(default)]
        path: Option<String>,
    },
    DeltaLake {
        #[serde(default)]
        table_uri: Option<String>,
        #[serde(default)]
        storage_options: Option<HashMap<String, String>>,
        #[serde(default)]
        version: Option<i64>,
        #[serde(default)]
        filter: Option<String>,
    },
    Kafka {
        #[serde(default)]
        brokers: Option<String>,
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        auto_offset_reset: Option<String>,
        #[serde(default)]
        security_protocol: Option<String>,
        #[serde(default)]
        sasl_mechanism: Option<String>,
        #[serde(default)]
        sasl_username: Option<String>,
        #[serde(default)]
        sasl_password: Option<String>,
        #[serde(default)]
        mode: Option<String>,
    },
    Sqs {
        #[serde(default)]
        queue_url: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        endpoint_url: Option<String>,
        #[serde(default)]
        mode: Option<String>,
    },
    Kinesis {
        #[serde(default)]
        stream_name: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        endpoint_url: Option<String>,
        #[serde(default)]
        mode: Option<String>,
    },
    Amqp {
        #[serde(default)]
        connection_string: Option<String>,
        #[serde(default)]
        queue: Option<String>,
        #[serde(default)]
        exchange: Option<String>,
        #[serde(default)]
        routing_key: Option<String>,
        #[serde(default)]
        prefetch_count: Option<u32>,
        #[serde(default)]
        mode: Option<String>,
    },
    Sns {
        #[serde(default)]
        topic_arn: Option<String>,
        #[serde(default)]
        sqs_queue_url: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        endpoint_url: Option<String>,
    },
    Eventbridge {
        #[serde(default)]
        event_bus_name: Option<String>,
        #[serde(default)]
        sqs_queue_url: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        endpoint_url: Option<String>,
    },
    Mqtt {
        #[serde(default)]
        broker_url: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        qos: Option<u8>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        mode: Option<String>,
    },
    Websocket {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
        #[serde(default)]
        mode: Option<String>,
    },
    #[serde(rename = "google_analytics")]
    GoogleAnalytics {
        #[serde(default)]
        property_id: Option<String>,
        #[serde(default)]
        start_date: Option<String>,
        #[serde(default)]
        end_date: Option<String>,
        #[serde(default)]
        lookback_days: Option<u32>,
        #[serde(default)]
        stream_profile: Option<String>,
        #[serde(default)]
        keep_empty_rows: Option<bool>,
        #[serde(default)]
        processing_lag_days: Option<u32>,
        #[serde(default)]
        window_in_days: Option<u32>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        oauth_token_url: Option<String>,
        #[serde(default)]
        oauth_client_id: Option<String>,
        #[serde(default)]
        oauth_client_secret: Option<String>,
        #[serde(default)]
        oauth_refresh_token: Option<String>,
        #[serde(default)]
        service_account_json_path: Option<String>,
        #[serde(default)]
        streams: Option<Vec<String>>,
    },
    #[serde(rename = "google_search_console")]
    GoogleSearchConsole {
        #[serde(default)]
        site_url: Option<String>,
        #[serde(default)]
        start_date: Option<String>,
        #[serde(default)]
        end_date: Option<String>,
        #[serde(default)]
        lookback_days: Option<u32>,
        #[serde(default)]
        stream_profile: Option<String>,
        #[serde(default)]
        processing_lag_days: Option<u32>,
        #[serde(default)]
        window_in_days: Option<u32>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        oauth_token_url: Option<String>,
        #[serde(default)]
        oauth_client_id: Option<String>,
        #[serde(default)]
        oauth_client_secret: Option<String>,
        #[serde(default)]
        oauth_refresh_token: Option<String>,
        #[serde(default)]
        service_account_json_path: Option<String>,
        #[serde(default)]
        streams: Option<Vec<String>>,
        #[serde(default)]
        search_type: Option<String>,
        #[serde(default)]
        data_state: Option<String>,
        #[serde(default)]
        row_limit: Option<u32>,
        #[serde(default)]
        url_inspection_enabled: Option<bool>,
        #[serde(default)]
        url_list: Option<Vec<String>>,
    },
    #[serde(rename = "bing_webmaster_tools")]
    BingWebmasterTools {
        #[serde(default)]
        site_url: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        start_date: Option<String>,
        #[serde(default)]
        end_date: Option<String>,
        #[serde(default)]
        lookback_days: Option<u32>,
        #[serde(default)]
        stream_profile: Option<String>,
        #[serde(default)]
        processing_lag_days: Option<u32>,
        #[serde(default)]
        window_in_days: Option<u32>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        oauth_token_url: Option<String>,
        #[serde(default)]
        oauth_client_id: Option<String>,
        #[serde(default)]
        oauth_client_secret: Option<String>,
        #[serde(default)]
        oauth_refresh_token: Option<String>,
        #[serde(default)]
        streams: Option<Vec<String>>,
    },
    #[serde(rename = "seo_crawl")]
    SeoCrawl {
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        max_urls: Option<u32>,
        #[serde(default)]
        max_depth: Option<u32>,
        #[serde(default)]
        crawl_rate_per_second: Option<f64>,
        #[serde(default)]
        respect_robots: Option<bool>,
        #[serde(default)]
        openai_enabled: Option<bool>,
        #[serde(default)]
        openai_model: Option<String>,
        #[serde(default)]
        openai_analyze_blocks: Option<bool>,
        #[serde(default)]
        openai_max_blocks_per_page: Option<u32>,
        #[serde(default)]
        skip_unchanged_content: Option<bool>,
        #[serde(default)]
        user_agent: Option<String>,
    },
    #[serde(rename = "google_serp_ranks")]
    GoogleSerpRanks {
        #[serde(default)]
        targets: Option<Vec<GoogleSerpTargetConfig>>,
        #[serde(default)]
        keywords: Option<Vec<String>>,
        #[serde(default)]
        country: Option<String>,
        #[serde(default)]
        language: Option<String>,
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        max_depth: Option<u32>,
        #[serde(default)]
        min_query_interval_ms: Option<u64>,
        #[serde(default)]
        max_queries_per_run: Option<u32>,
        #[serde(default)]
        stop_after_first_target_match: Option<bool>,
        #[serde(default)]
        capture_results: Option<bool>,
        #[serde(default)]
        force_refresh_today: Option<bool>,
        #[serde(default)]
        navigation_timeout_ms: Option<u32>,
        #[serde(default)]
        worker_node_path: Option<String>,
        #[serde(default)]
        playwright_executable_path: Option<String>,
        #[serde(default)]
        user_agent: Option<String>,
    },
    #[serde(rename = "ai_citations")]
    AiCitations {
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        brand_names: Option<Vec<String>>,
        #[serde(default)]
        prompt_list: Option<Vec<TrackedPromptEntry>>,
        #[serde(default)]
        models: Option<Vec<String>>,
        #[serde(default)]
        requests_per_minute: Option<u32>,
        #[serde(default)]
        max_prompts_per_run: Option<u32>,
        #[serde(default)]
        skip_unchanged_responses: Option<bool>,
        #[serde(default)]
        openai_base_url: Option<String>,
    },
    #[serde(rename = "site_quality")]
    SiteQuality {
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        url_mode: Option<String>,
        #[serde(default)]
        url_list: Option<Vec<String>>,
        #[serde(default)]
        max_pages_per_run: Option<u32>,
        #[serde(default)]
        wait_until: Option<String>,
        #[serde(default)]
        navigation_timeout_ms: Option<u32>,
        #[serde(default)]
        lighthouse_enabled: Option<bool>,
        #[serde(default)]
        lighthouse_categories: Option<Vec<String>>,
        #[serde(default)]
        axe_enabled: Option<bool>,
        #[serde(default)]
        axe_tags: Option<Vec<String>>,
        #[serde(default)]
        pages_per_minute: Option<u32>,
        #[serde(default)]
        worker_node_path: Option<String>,
        #[serde(default)]
        playwright_executable_path: Option<String>,
        #[serde(default)]
        respect_robots: Option<bool>,
        #[serde(default)]
        skip_heavy_when_unchanged: Option<bool>,
    },
    #[serde(rename = "google_pagespeed")]
    GooglePageSpeed {
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        url_mode: Option<String>,
        #[serde(default)]
        url_list: Option<Vec<String>>,
        #[serde(default)]
        max_urls: Option<u32>,
        #[serde(default)]
        strategies: Option<Vec<String>>,
        #[serde(default)]
        categories: Option<Vec<String>>,
        #[serde(default)]
        locale: Option<String>,
        #[serde(default)]
        max_requests_per_run: Option<u32>,
        #[serde(default)]
        requests_per_minute: Option<u32>,
        #[serde(default)]
        respect_robots: Option<bool>,
        #[serde(default)]
        top_audits_per_page: Option<u32>,
        #[serde(default)]
        max_concurrent_requests: Option<u32>,
    },
    #[serde(rename = "apple_search_ads")]
    AppleSearchAds {
        #[serde(default)]
        org_id: Option<String>,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        key_id: Option<String>,
        #[serde(default)]
        private_key_path: Option<String>,
        #[serde(default)]
        private_key_pem: Option<String>,
        #[serde(default)]
        start_date: Option<String>,
        #[serde(default)]
        end_date: Option<String>,
        #[serde(default)]
        lookback_days: Option<u32>,
        #[serde(default)]
        stream_profile: Option<String>,
        #[serde(default)]
        processing_lag_days: Option<u32>,
        #[serde(default)]
        time_zone: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        streams: Option<Vec<String>>,
        #[serde(default)]
        return_records_with_no_metrics: Option<bool>,
        #[serde(default)]
        max_concurrent_requests: Option<u32>,
    },
    #[serde(rename = "dataforseo_backlinks")]
    DataForSeoBacklinks {
        #[serde(default)]
        login: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        run_mode: Option<String>,
        #[serde(default)]
        backlink_target: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        max_pages: Option<u32>,
        #[serde(default)]
        request_interval_ms: Option<u64>,
    },
    #[serde(rename = "dataforseo_seo_opportunities")]
    DataForSeoSeoOpportunities {
        #[serde(default)]
        login: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        site: Option<String>,
        #[serde(default)]
        location_code: Option<u32>,
        #[serde(default)]
        language_code: Option<String>,
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        run_mode: Option<String>,
        #[serde(default)]
        seed_keywords: Option<Vec<String>>,
        #[serde(default)]
        request_interval_ms: Option<u64>,
    },
    #[serde(rename = "meta_instagram_ads")]
    MetaInstagramAds {
        #[serde(default)]
        ad_account_id: Option<String>,
        #[serde(default)]
        start_date: Option<String>,
        #[serde(default)]
        end_date: Option<String>,
        #[serde(default)]
        lookback_days: Option<u32>,
        #[serde(default)]
        stream_profile: Option<String>,
        #[serde(default)]
        processing_lag_days: Option<u32>,
        #[serde(default)]
        api_version: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        oauth_token_url: Option<String>,
        #[serde(default)]
        oauth_client_id: Option<String>,
        #[serde(default)]
        oauth_client_secret: Option<String>,
        #[serde(default)]
        oauth_refresh_token: Option<String>,
        #[serde(default)]
        instagram_filter: Option<bool>,
        #[serde(default)]
        streams: Option<Vec<String>>,
    },
    HttpClient {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        auth_strategy: Option<String>,
        #[serde(default)]
        auth_user: Option<String>,
        #[serde(default)]
        auth_password: Option<String>,
        #[serde(default)]
        auth_token: Option<String>,
        #[serde(default)]
        scrape_interval_seconds: Option<u64>,
    },
    HttpServer {
        #[serde(default)]
        listen_address: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        auth_token: Option<String>,
    },
    Socket {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        address: Option<String>,
        #[serde(default)]
        framing: Option<String>,
    },
    Statsd {
        #[serde(default)]
        listen_address: Option<String>,
    },
    Stdin {
        #[serde(default)]
        mode: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct S3Transform {
    #[serde(default)]
    pub namespace_fields: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtConfig {
    #[serde(default)]
    pub target_schema: Option<String>,
    #[serde(default)]
    pub silver_suffix: Option<String>,
    #[serde(default)]
    pub gold_suffix: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaSinkConfig {
    Glue { glue_database_name: String },
}

impl SkipprProjectConfig {
    pub fn load_from(path: &Path) -> Result<Self, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        serde_yaml::from_slice::<Self>(&bytes)
            .map_err(|e| format!("failed to parse {}: {}", path.display(), e))
    }

    /// Like [`load_from`], but resolves `${VAR}` placeholders in the YAML (same as engine config).
    pub fn load_resolved_from(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        let value: serde_yaml::Value = serde_yaml::from_str(&raw)
            .map_err(|e| format!("failed to parse {}: {}", path.display(), e))?;
        let mut json_value = serde_json::to_value(&value)
            .map_err(|e| format!("failed to normalize {}: {}", path.display(), e))?;
        skipprd::helpers::configuration::Config::resolve_env_refs_in_json_value(&mut json_value)?;
        let yaml_back = serde_yaml::to_value(&json_value)
            .map_err(|e| format!("failed to convert resolved {}: {}", path.display(), e))?;
        serde_yaml::from_value(yaml_back)
            .map_err(|e| format!("failed to parse resolved {}: {}", path.display(), e))
    }

    /// Read `pipelines.<pipeline_name>.vector_source` (and optional chunk fields) for `vector ingest-docs`.
    pub fn vector_ingest_pipeline_spec(
        &self,
        pipeline_name: &str,
    ) -> Result<VectorIngestPipelineSpec, String> {
        let name = pipeline_name.trim();
        if name.is_empty() {
            return Err("pipeline name must not be empty".to_string());
        }
        let raw = self.pipelines.get(name).ok_or_else(|| {
            format!(
                "skippr.yml has no pipelines.{name} entry; add a mapping with vector_source: <vector_sources key>, or pass --pipeline <name>"
            )
        })?;
        let m = raw
            .as_mapping()
            .ok_or_else(|| format!("pipelines.{name} must be a mapping (got non-object YAML)"))?;
        let vector_source = yaml_mapping_get_str(m, "vector_source")
            .ok_or_else(|| {
                format!(
                    "pipelines.{name} must set vector_source: <key> matching an entry under vector_sources"
                )
            })?
            .to_string();
        Ok(VectorIngestPipelineSpec {
            vector_source,
            chunk_chars: yaml_mapping_get_usize(m, "chunk_chars"),
            chunk_overlap: yaml_mapping_get_usize(m, "chunk_overlap"),
        })
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let yaml = serde_yaml::to_string(self)
            .map_err(|e| format!("failed to serialize config: {}", e))?;
        std::fs::write(path, yaml.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))
    }

    pub fn warehouse_kind_str(&self) -> Option<&'static str> {
        match &self.warehouse {
            Some(WarehouseConfig::Athena { .. }) => Some("athena"),
            Some(WarehouseConfig::Snowflake { .. }) => Some("snowflake"),
            Some(WarehouseConfig::Bigquery { .. }) => Some("bigquery"),
            Some(WarehouseConfig::Postgres { .. }) => Some("postgres"),
            Some(WarehouseConfig::Databricks { .. }) => Some("databricks"),
            Some(WarehouseConfig::Synapse { .. }) => Some("synapse"),
            Some(WarehouseConfig::Redshift { .. }) => Some("redshift"),
            Some(WarehouseConfig::Clickhouse { .. }) => Some("clickhouse"),
            Some(WarehouseConfig::Motherduck { .. }) => Some("motherduck"),
            None => None,
        }
    }

    pub fn source_kind_str(&self) -> Option<&'static str> {
        match &self.source {
            Some(SourceConfig::Mssql { .. }) => Some("mssql"),
            Some(SourceConfig::S3 { .. }) => Some("s3"),
            Some(SourceConfig::Mysql { .. }) => Some("mysql"),
            Some(SourceConfig::PostgresSource { .. }) => Some("postgres_source"),
            Some(SourceConfig::RedshiftSource { .. }) => Some("redshift_source"),
            Some(SourceConfig::Mongodb { .. }) => Some("mongodb"),
            Some(SourceConfig::Dynamodb { .. }) => Some("dynamodb"),
            Some(SourceConfig::ClickhouseSource { .. }) => Some("clickhouse_source"),
            Some(SourceConfig::MotherduckSource { .. }) => Some("motherduck_source"),
            Some(SourceConfig::Sftp { .. }) => Some("sftp"),
            Some(SourceConfig::File { .. }) => Some("file"),
            Some(SourceConfig::DeltaLake { .. }) => Some("delta_lake"),
            Some(SourceConfig::Kafka { .. }) => Some("kafka"),
            Some(SourceConfig::Sqs { .. }) => Some("sqs"),
            Some(SourceConfig::Kinesis { .. }) => Some("kinesis"),
            Some(SourceConfig::Amqp { .. }) => Some("amqp"),
            Some(SourceConfig::Sns { .. }) => Some("sns"),
            Some(SourceConfig::Eventbridge { .. }) => Some("eventbridge"),
            Some(SourceConfig::Mqtt { .. }) => Some("mqtt"),
            Some(SourceConfig::Websocket { .. }) => Some("websocket"),
            Some(SourceConfig::GoogleAnalytics { .. }) => Some("google_analytics"),
            Some(SourceConfig::GoogleSearchConsole { .. }) => Some("google_search_console"),
            Some(SourceConfig::BingWebmasterTools { .. }) => Some("bing_webmaster_tools"),
            Some(SourceConfig::GooglePageSpeed { .. }) => Some("google_pagespeed"),
            Some(SourceConfig::SeoCrawl { .. }) => Some("seo_crawl"),
            Some(SourceConfig::GoogleSerpRanks { .. }) => Some("google_serp_ranks"),
            Some(SourceConfig::AiCitations { .. }) => Some("ai_citations"),
            Some(SourceConfig::SiteQuality { .. }) => Some("site_quality"),
            Some(SourceConfig::AppleSearchAds { .. }) => Some("apple_search_ads"),
            Some(SourceConfig::MetaInstagramAds { .. }) => Some("meta_instagram_ads"),
            Some(SourceConfig::DataForSeoBacklinks { .. }) => Some("dataforseo_backlinks"),
            Some(SourceConfig::DataForSeoSeoOpportunities { .. }) => {
                Some("dataforseo_seo_opportunities")
            }
            Some(SourceConfig::HttpClient { .. }) => Some("http_client"),
            Some(SourceConfig::HttpServer { .. }) => Some("http_server"),
            Some(SourceConfig::Socket { .. }) => Some("socket"),
            Some(SourceConfig::Statsd { .. }) => Some("statsd"),
            Some(SourceConfig::Stdin { .. }) => Some("stdin"),
            None => None,
        }
    }
}

impl WarehouseConfig {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Athena { .. } => "athena",
            Self::Snowflake { .. } => "snowflake",
            Self::Bigquery { .. } => "bigquery",
            Self::Postgres { .. } => "postgres",
            Self::Databricks { .. } => "databricks",
            Self::Synapse { .. } => "synapse",
            Self::Redshift { .. } => "redshift",
            Self::Clickhouse { .. } => "clickhouse",
            Self::Motherduck { .. } => "motherduck",
        }
    }
}
