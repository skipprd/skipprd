use react::config::{LlmFile, ReactConfigFile, ScopeFile, StorageFile};
use react_core::resolved_config::S3Credentials;

use crate::public_config::{SkipprDbtConfig, SourceConfig, WarehouseConfig};

const DEFAULT_LLM_BASE_URL: &str = "https://api.openai.com";

/// Translate the public `skippr` config into the internal runtime config.
pub fn to_internal(cfg: &SkipprDbtConfig) -> Result<ReactConfigFile, String> {
    let project = cfg.project.trim();
    if project.is_empty() {
        return Err("project name is required in skippr.yaml".to_string());
    }

    let warehouse_json = match &cfg.warehouse {
        Some(WarehouseConfig::Athena {
            workgroup,
            region,
            result_s3,
            schema,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "athena".into());
            if let Some(v) = workgroup {
                m.insert("workgroup".into(), v.clone().into());
            }
            if let Some(v) = region {
                m.insert("region".into(), v.clone().into());
            }
            if let Some(v) = result_s3 {
                m.insert("result_s3".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Snowflake {
            account,
            user,
            password,
            private_key_path,
            stage,
            staging_uri,
            staging_storage_integration,
            staging_azure_sas_token,
            staging_azure_account_key,
            staging_gcs_service_account_key_path,
            database,
            schema,
            warehouse,
            role,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "snowflake".into());
            if let Some(v) = account {
                m.insert("account".into(), v.clone().into());
            }
            if let Some(v) = user {
                m.insert("user".into(), v.clone().into());
            }
            if let Some(v) = password {
                m.insert("password".into(), v.clone().into());
            }
            if let Some(v) = private_key_path {
                m.insert("private_key_path".into(), v.clone().into());
            }
            if let Some(v) = stage {
                m.insert("stage".into(), v.clone().into());
            }
            if let Some(v) = staging_uri {
                m.insert("staging_uri".into(), v.clone().into());
            }
            if let Some(v) = staging_storage_integration {
                m.insert("staging_storage_integration".into(), v.clone().into());
            }
            if let Some(v) = staging_azure_sas_token {
                m.insert("staging_azure_sas_token".into(), v.clone().into());
            }
            if let Some(v) = staging_azure_account_key {
                m.insert("staging_azure_account_key".into(), v.clone().into());
            }
            if let Some(v) = staging_gcs_service_account_key_path {
                m.insert(
                    "staging_gcs_service_account_key_path".into(),
                    v.clone().into(),
                );
            }
            if let Some(v) = database {
                m.insert("database".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            if let Some(v) = warehouse {
                m.insert("warehouse".into(), v.clone().into());
            }
            if let Some(v) = role {
                m.insert("role".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Bigquery {
            project,
            dataset,
            location,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "bigquery".into());
            if let Some(v) = project {
                m.insert("project".into(), v.clone().into());
            }
            if let Some(v) = dataset {
                m.insert("dataset".into(), v.clone().into());
            }
            if let Some(v) = location {
                m.insert("location".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Postgres { database, schema }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "postgres".into());
            if let Some(v) = database {
                m.insert("database".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Databricks {
            workspace_url,
            token,
            warehouse_id,
            catalog,
            schema,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "databricks".into());
            if let Some(v) = workspace_url {
                m.insert("workspace_url".into(), v.clone().into());
            }
            if let Some(v) = token {
                m.insert("token".into(), v.clone().into());
            }
            if let Some(v) = warehouse_id {
                m.insert("warehouse_id".into(), v.clone().into());
            }
            if let Some(v) = catalog {
                m.insert("catalog".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Synapse {
            connection_string,
            schema,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "synapse".into());
            if let Some(v) = connection_string {
                m.insert("connection_string".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Redshift {
            database,
            cluster_identifier,
            workgroup_name,
            db_user,
            schema,
            region,
            staging_s3_bucket,
            staging_s3_prefix,
            iam_role_arn,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "redshift".into());
            if let Some(v) = database {
                m.insert("database".into(), v.clone().into());
            }
            if let Some(v) = cluster_identifier {
                m.insert("cluster_identifier".into(), v.clone().into());
            }
            if let Some(v) = workgroup_name {
                m.insert("workgroup_name".into(), v.clone().into());
            }
            if let Some(v) = db_user {
                m.insert("db_user".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            if let Some(v) = region {
                m.insert("region".into(), v.clone().into());
            }
            if let Some(v) = staging_s3_bucket {
                m.insert("staging_s3_bucket".into(), v.clone().into());
            }
            if let Some(v) = staging_s3_prefix {
                m.insert("staging_s3_prefix".into(), v.clone().into());
            }
            if let Some(v) = iam_role_arn {
                m.insert("iam_role_arn".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Clickhouse {
            url,
            database,
            user,
            password,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "clickhouse".into());
            if let Some(v) = url {
                m.insert("url".into(), v.clone().into());
            }
            if let Some(v) = database {
                m.insert("database".into(), v.clone().into());
            }
            if let Some(v) = user {
                m.insert("user".into(), v.clone().into());
            }
            if let Some(v) = password {
                m.insert("password".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        Some(WarehouseConfig::Motherduck {
            motherduck_token,
            database,
            schema,
        }) => {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "motherduck".into());
            if let Some(v) = motherduck_token {
                m.insert("motherduck_token".into(), v.clone().into());
            }
            if let Some(v) = database {
                m.insert("database".into(), v.clone().into());
            }
            if let Some(v) = schema {
                m.insert("schema".into(), v.clone().into());
            }
            serde_json::Value::Object(m)
        }
        None => {
            return Err(
                "warehouse is not configured. Run: skippr connect warehouse <kind>".to_string(),
            );
        }
    };

    let dbt_target = cfg.warehouse_kind_str().unwrap_or_default().to_string();

    let dbt_cfg = cfg.dbt.clone().unwrap_or_default();
    let target_schema = dbt_cfg
        .target_schema
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| project.to_string());
    let silver_suffix = dbt_cfg
        .silver_suffix
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "silver".to_string());
    let gold_suffix = dbt_cfg
        .gold_suffix
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gold".to_string());

    let el_json = match &cfg.source {
        Some(source) => {
            let skippr_input = match source {
                SourceConfig::Mssql { connection_string } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "mssql".into());
                    if let Some(cs) = connection_string {
                        m.insert("connection_string".into(), cs.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::S3 {
                    s3_bucket,
                    s3_prefix,
                    transform,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "s3".into());
                    if let Some(v) = s3_bucket {
                        m.insert("s3_bucket".into(), v.clone().into());
                    }
                    if let Some(v) = s3_prefix {
                        m.insert("s3_prefix".into(), v.clone().into());
                    }
                    if let Some(t) = transform {
                        let mut tm = serde_json::Map::new();
                        if let Some(nf) = &t.namespace_fields {
                            tm.insert("namespace_fields".into(), nf.clone().into());
                        }
                        if !tm.is_empty() {
                            m.insert("transform".into(), serde_json::Value::Object(tm));
                        }
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Mysql {
                    connection_string,
                    tables,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "mysql".into());
                    if let Some(v) = connection_string {
                        m.insert("connection_string".into(), v.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::PostgresSource {
                    host,
                    port,
                    user,
                    password,
                    database,
                    connection_string,
                    tables,
                    query,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "postgres".into());
                    if let Some(v) = host {
                        m.insert("host".into(), v.clone().into());
                    }
                    if let Some(v) = port {
                        m.insert("port".into(), (*v).into());
                    }
                    if let Some(v) = user {
                        m.insert("user".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = database {
                        m.insert("database".into(), v.clone().into());
                    }
                    if let Some(v) = connection_string {
                        m.insert("connection_string".into(), v.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    if let Some(v) = query {
                        m.insert("query".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::RedshiftSource {
                    cluster_identifier,
                    workgroup_name,
                    database,
                    db_user,
                    tables,
                    region,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "redshift".into());
                    if let Some(v) = cluster_identifier {
                        m.insert("cluster_identifier".into(), v.clone().into());
                    }
                    if let Some(v) = workgroup_name {
                        m.insert("workgroup_name".into(), v.clone().into());
                    }
                    if let Some(v) = database {
                        m.insert("database".into(), v.clone().into());
                    }
                    if let Some(v) = db_user {
                        m.insert("db_user".into(), v.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Mongodb {
                    connection_string,
                    database,
                    collection,
                    filter,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "mongodb".into());
                    if let Some(v) = connection_string {
                        m.insert("connection_string".into(), v.clone().into());
                    }
                    if let Some(v) = database {
                        m.insert("database".into(), v.clone().into());
                    }
                    if let Some(v) = collection {
                        m.insert("collection".into(), v.clone().into());
                    }
                    if let Some(v) = filter {
                        m.insert("filter".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Dynamodb {
                    table_name,
                    region,
                    endpoint_url,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "dynamodb".into());
                    if let Some(v) = table_name {
                        m.insert("table_name".into(), v.clone().into());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    if let Some(v) = endpoint_url {
                        m.insert("endpoint_url".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::ClickhouseSource {
                    url,
                    database,
                    user,
                    password,
                    tables,
                    query,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "clickhouse".into());
                    if let Some(v) = url {
                        m.insert("url".into(), v.clone().into());
                    }
                    if let Some(v) = database {
                        m.insert("database".into(), v.clone().into());
                    }
                    if let Some(v) = user {
                        m.insert("user".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    if let Some(v) = query {
                        m.insert("query".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::MotherduckSource {
                    motherduck_token,
                    database,
                    tables,
                    query,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "motherduck".into());
                    if let Some(v) = motherduck_token {
                        m.insert("motherduck_token".into(), v.clone().into());
                    }
                    if let Some(v) = database {
                        m.insert("database".into(), v.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    if let Some(v) = query {
                        m.insert("query".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Sftp {
                    host,
                    port,
                    username,
                    password,
                    private_key_path,
                    remote_path,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "sftp".into());
                    if let Some(v) = host {
                        m.insert("host".into(), v.clone().into());
                    }
                    if let Some(v) = port {
                        m.insert("port".into(), (*v).into());
                    }
                    if let Some(v) = username {
                        m.insert("username".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = private_key_path {
                        m.insert("private_key_path".into(), v.clone().into());
                    }
                    if let Some(v) = remote_path {
                        m.insert("remote_path".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::File { path } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "file".into());
                    if let Some(v) = path {
                        m.insert("path".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::DeltaLake {
                    table_uri,
                    storage_options,
                    version,
                    filter,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "delta_lake".into());
                    if let Some(v) = table_uri {
                        m.insert("table_uri".into(), v.clone().into());
                    }
                    if let Some(opts) = storage_options {
                        m.insert(
                            "storage_options".into(),
                            serde_json::to_value(opts).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = version {
                        m.insert("version".into(), (*v).into());
                    }
                    if let Some(v) = filter {
                        m.insert("filter".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Kafka {
                    brokers,
                    topic,
                    group_id,
                    auto_offset_reset,
                    security_protocol,
                    sasl_mechanism,
                    sasl_username,
                    sasl_password,
                    mode,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "kafka".into());
                    if let Some(v) = brokers {
                        m.insert("brokers".into(), v.clone().into());
                    }
                    if let Some(v) = topic {
                        m.insert("topic".into(), v.clone().into());
                    }
                    if let Some(v) = group_id {
                        m.insert("group_id".into(), v.clone().into());
                    }
                    if let Some(v) = auto_offset_reset {
                        m.insert("auto_offset_reset".into(), v.clone().into());
                    }
                    if let Some(v) = security_protocol {
                        m.insert("security_protocol".into(), v.clone().into());
                    }
                    if let Some(v) = sasl_mechanism {
                        m.insert("sasl_mechanism".into(), v.clone().into());
                    }
                    if let Some(v) = sasl_username {
                        m.insert("sasl_username".into(), v.clone().into());
                    }
                    if let Some(v) = sasl_password {
                        m.insert("sasl_password".into(), v.clone().into());
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Sqs {
                    queue_url,
                    region,
                    endpoint_url,
                    mode,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "sqs".into());
                    if let Some(v) = queue_url {
                        m.insert("queue_url".into(), v.clone().into());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    if let Some(v) = endpoint_url {
                        m.insert("endpoint_url".into(), v.clone().into());
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Kinesis {
                    stream_name,
                    region,
                    endpoint_url,
                    mode,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "kinesis".into());
                    if let Some(v) = stream_name {
                        m.insert("stream_name".into(), v.clone().into());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    if let Some(v) = endpoint_url {
                        m.insert("endpoint_url".into(), v.clone().into());
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Amqp {
                    connection_string,
                    queue,
                    exchange,
                    routing_key,
                    prefetch_count,
                    mode,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "amqp".into());
                    if let Some(v) = connection_string {
                        m.insert("connection_string".into(), v.clone().into());
                    }
                    if let Some(v) = queue {
                        m.insert("queue".into(), v.clone().into());
                    }
                    if let Some(v) = exchange {
                        m.insert("exchange".into(), v.clone().into());
                    }
                    if let Some(v) = routing_key {
                        m.insert("routing_key".into(), v.clone().into());
                    }
                    if let Some(v) = prefetch_count {
                        m.insert("prefetch_count".into(), (*v).into());
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Sns {
                    topic_arn,
                    sqs_queue_url,
                    region,
                    endpoint_url,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "sns".into());
                    if let Some(v) = topic_arn {
                        m.insert("topic_arn".into(), v.clone().into());
                    }
                    if let Some(v) = sqs_queue_url {
                        m.insert("sqs_queue_url".into(), v.clone().into());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    if let Some(v) = endpoint_url {
                        m.insert("endpoint_url".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Eventbridge {
                    event_bus_name,
                    sqs_queue_url,
                    region,
                    endpoint_url,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "eventbridge".into());
                    if let Some(v) = event_bus_name {
                        m.insert("event_bus_name".into(), v.clone().into());
                    }
                    if let Some(v) = sqs_queue_url {
                        m.insert("sqs_queue_url".into(), v.clone().into());
                    }
                    if let Some(v) = region {
                        m.insert("region".into(), v.clone().into());
                    }
                    if let Some(v) = endpoint_url {
                        m.insert("endpoint_url".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Mqtt {
                    broker_url,
                    port,
                    topic,
                    client_id,
                    qos,
                    username,
                    password,
                    mode,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "mqtt".into());
                    if let Some(v) = broker_url {
                        m.insert("broker_url".into(), v.clone().into());
                    }
                    if let Some(v) = port {
                        m.insert("port".into(), (*v).into());
                    }
                    if let Some(v) = topic {
                        m.insert("topic".into(), v.clone().into());
                    }
                    if let Some(v) = client_id {
                        m.insert("client_id".into(), v.clone().into());
                    }
                    if let Some(v) = qos {
                        m.insert("qos".into(), (*v).into());
                    }
                    if let Some(v) = username {
                        m.insert("username".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Websocket { url, headers, mode } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "websocket".into());
                    if let Some(v) = url {
                        m.insert("url".into(), v.clone().into());
                    }
                    if let Some(v) = headers {
                        m.insert(
                            "headers".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::HttpClient {
                    url,
                    method,
                    headers,
                    body,
                    auth_strategy,
                    auth_user,
                    auth_password,
                    auth_token,
                    scrape_interval_seconds,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "http_client".into());
                    if let Some(v) = url {
                        m.insert("url".into(), v.clone().into());
                    }
                    if let Some(v) = method {
                        m.insert("method".into(), v.clone().into());
                    }
                    if let Some(v) = headers {
                        m.insert(
                            "headers".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = body {
                        m.insert("body".into(), v.clone().into());
                    }
                    let mut auth = serde_json::Map::new();
                    if let Some(v) = auth_strategy {
                        auth.insert("strategy".into(), v.clone().into());
                    }
                    if let Some(v) = auth_user {
                        auth.insert("user".into(), v.clone().into());
                    }
                    if let Some(v) = auth_password {
                        auth.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = auth_token {
                        auth.insert("token".into(), v.clone().into());
                    }
                    if !auth.is_empty() {
                        m.insert("auth".into(), serde_json::Value::Object(auth));
                    }
                    if let Some(v) = scrape_interval_seconds {
                        m.insert("scrape_interval_seconds".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::HttpServer {
                    listen_address,
                    path,
                    auth_token,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "http_server".into());
                    if let Some(v) = listen_address {
                        m.insert("listen_address".into(), v.clone().into());
                    }
                    if let Some(v) = path {
                        m.insert("path".into(), v.clone().into());
                    }
                    if let Some(v) = auth_token {
                        m.insert("auth_token".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Socket {
                    mode,
                    address,
                    framing,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "socket".into());
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    if let Some(v) = address {
                        m.insert("address".into(), v.clone().into());
                    }
                    if let Some(v) = framing {
                        m.insert("framing".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Statsd { listen_address } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "statsd".into());
                    if let Some(v) = listen_address {
                        m.insert("listen_address".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::Stdin { mode } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "stdin".into());
                    if let Some(v) = mode {
                        m.insert("mode".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
            };
            let mut el = serde_json::json!({
                "enabled": true,
                "skippr_input": skippr_input,
            });
            if let Some(ref ss) = cfg.schema_sink {
                let ss_json = match ss {
                    crate::public_config::SchemaSinkConfig::Glue { glue_database_name } => {
                        serde_json::json!({ "kind": "glue", "glue_database_name": glue_database_name })
                    }
                };
                el["schema_sink"] = ss_json;
            }
            el
        }
        None => serde_json::json!({ "enabled": false }),
    };

    let providers = serde_json::json!({
        "warehouse": warehouse_json,
        "el": el_json,
        "catalog": {
            "enabled": true,
            "refresh_secs": 3600,
            "max_concurrency": 8,
        },
        "dbt": {
            "enabled": true,
            "runner": "host",
            "target": dbt_target,
            "naming": {
                "target_schema": target_schema,
                "silver_suffix": silver_suffix,
                "gold_suffix": gold_suffix,
            },
        },
        "vector": {
            "enabled": true,
        },
    });

    Ok(ReactConfigFile {
        version: Some(1),
        server: None,
        storage: Some(StorageFile {
            mode: Some("local".into()),
            bucket: None,
            path: Some("./.skippr".into()),
            s3_credentials: None,
        }),
        scope: Some(ScopeFile {
            tenant: Some("_".into()),
            workspace: Some("dev".into()),
            project_id: Some(project.to_string()),
        }),
        llm: Some(LlmFile {
            provider: Some("OPENAI_COMPAT".into()),
            base_url: Some(DEFAULT_LLM_BASE_URL.into()),
            reason_model: Some("gpt-5.4".into()),
            task_model: Some("gpt-5.4".into()),
            embed_model: Some("text-embedding-3-small".into()),
            context_length: Some(8192),
            http_timeout_secs: Some(120),
            max_tokens: Some(8192),
            temperature: Some(0.2),
            top_p: Some(1.0),
            ..Default::default()
        }),
        providers: Some(providers),
    })
}

/// Set the skippr binary path in the EL provider config.
pub fn set_skippr_binary(cfg: &mut ReactConfigFile, binary_path: &str) {
    if let Some(ref mut providers) = cfg.providers {
        if let Some(el) = providers.get_mut("el") {
            el["skippr_binary"] = serde_json::Value::String(binary_path.to_string());
        }
    }
}

pub fn s3_credentials_from_auth(creds: &crate::api_client::CredentialsResponse) -> S3Credentials {
    let expires_at = chrono::DateTime::parse_from_rfc3339(&creds.credentials.expiration)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    S3Credentials {
        access_key_id: creds.credentials.access_key_id.clone(),
        secret_access_key: creds.credentials.secret_access_key.clone(),
        session_token: Some(creds.credentials.session_token.clone()),
        region: "us-east-1".to_string(),
        expires_at,
        provider: None,
    }
}

/// Overlay authenticated mode onto an existing config:
/// - Switch storage to S3 with STS credentials
/// - Set server-provided LLM API key (if user hasn't set their own)
/// - Initialize metering client (sharing the same TokenProvider as ApiClient)
pub fn apply_authenticated_overlay(
    cfg: &mut ReactConfigFile,
    creds: &crate::api_client::CredentialsResponse,
    tokens: std::sync::Arc<react_suite_data_engineer::metering::TokenProvider>,
    initial_balance: f64,
) -> Result<(), String> {
    cfg.storage = Some(StorageFile {
        mode: Some("s3".into()),
        bucket: Some(creds.bucket.clone()),
        path: None,
        s3_credentials: Some(s3_credentials_from_auth(creds)),
    });

    if let Some(scope) = cfg.scope.as_mut() {
        if !creds.tenant_id.trim().is_empty() {
            scope.tenant = Some(creds.tenant_id.clone());
        }
    }

    let llm = cfg.llm.get_or_insert_with(Default::default);
    llm.provider.get_or_insert_with(|| "OPENAI_COMPAT".into());
    if llm
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        llm.base_url = Some(DEFAULT_LLM_BASE_URL.into());
    }
    if std::env::var("LLM_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_none()
    {
        std::env::set_var("LLM_BASE_URL", DEFAULT_LLM_BASE_URL);
    }

    let existing_llm_key = std::env::var("LLM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if existing_llm_key.is_none() {
        let server_key = creds.llm_api_key.trim();
        if server_key.is_empty() {
            return Err(
                "Skippr did not return an LLM token for this authenticated session.".to_string(),
            );
        }
        std::env::set_var("LLM_API_KEY", server_key);
    }

    let accounting_url = if creds.accounting_url.is_empty() {
        None
    } else {
        Some(creds.accounting_url.clone())
    };

    react_suite_data_engineer::metering::init_metering(accounting_url, tokens, initial_balance);

    react::llm::set_llm_usage_handler(Box::new(|usage: react::llm::LlmUsage| {
        react_suite_data_engineer::metering::report_llm_usage(
            react_suite_data_engineer::metering::LlmUsageRecord {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                model: usage.model,
                kind: match usage.kind {
                    react::llm::LlmUsageKind::Chat => {
                        react_suite_data_engineer::metering::LlmRequestKind::Chat
                    }
                    react::llm::LlmUsageKind::Embed => {
                        react_suite_data_engineer::metering::LlmRequestKind::Embed
                    }
                },
                project_id: usage.project_id,
                thread_id: usage.thread_id,
                prompt_id: usage.prompt_id,
                provider_usage: usage.provider_usage,
            },
        );
    }));

    react::llm::set_llm_pre_call_guard(Box::new(|| {
        react_suite_data_engineer::metering::check_budget()
    }));

    Ok(())
}

/// Minimal internal react config for `skippr vector ingest-docs` (vector + metering only).
/// Uses a placeholder Postgres warehouse so suite YAML resolves; warehouse providers are never started
/// because the CLI builds [`react::bootstrap::build_base_suite_ctx`] and attaches Lance only.
pub fn react_config_file_for_vector_doc_ingest(
    project_id: impl AsRef<str>,
    workspace: impl AsRef<str>,
) -> ReactConfigFile {
    let project_id = project_id.as_ref();
    let workspace = workspace.as_ref();
    let providers = serde_json::json!({
        "warehouse": {
            "kind": "postgres",
            "database": "_skippr_vector_ingest_noop",
            "schema": "public"
        },
        "catalog": { "enabled": false },
        "dbt": { "enabled": false },
        "vector": { "enabled": true },
        "el": { "enabled": false },
    });
    ReactConfigFile {
        version: Some(1),
        server: None,
        storage: Some(StorageFile {
            mode: Some("local".into()),
            bucket: None,
            path: Some("./.skippr".into()),
            s3_credentials: None,
        }),
        scope: Some(ScopeFile {
            tenant: Some("_".into()),
            workspace: Some(workspace.to_string()),
            project_id: Some(project_id.to_string()),
        }),
        llm: Some(LlmFile {
            provider: Some("OPENAI_COMPAT".into()),
            base_url: Some(DEFAULT_LLM_BASE_URL.into()),
            reason_model: Some("gpt-5.4".into()),
            task_model: Some("gpt-5.4".into()),
            embed_model: Some("text-embedding-3-small".into()),
            context_length: Some(8192),
            http_timeout_secs: Some(120),
            max_tokens: Some(8192),
            temperature: Some(0.2),
            top_p: Some(1.0),
            ..Default::default()
        }),
        providers: Some(providers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_config::*;
    use std::sync::Mutex;

    static LLM_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_clean_llm_env<F: FnOnce() + std::panic::UnwindSafe>(body: F) {
        let _guard = LLM_ENV_LOCK.lock().unwrap();
        let saved_api_key = std::env::var("LLM_API_KEY").ok();
        let saved_base_url = std::env::var("LLM_BASE_URL").ok();
        std::env::remove_var("LLM_API_KEY");
        std::env::remove_var("LLM_BASE_URL");

        let result = std::panic::catch_unwind(body);

        match saved_api_key {
            Some(value) => std::env::set_var("LLM_API_KEY", value),
            None => std::env::remove_var("LLM_API_KEY"),
        }
        match saved_base_url {
            Some(value) => std::env::set_var("LLM_BASE_URL", value),
            None => std::env::remove_var("LLM_BASE_URL"),
        }

        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    fn mssql_snowflake_config() -> SkipprDbtConfig {
        SkipprDbtConfig {
            project: "tes".into(),
            warehouse: Some(WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: Some("ANALYTICS".into()),
                schema: Some("RAW".into()),
                warehouse: Some("COMPUTE_WH".into()),
                role: Some("ACCOUNTADMIN".into()),
            }),
            source: Some(SourceConfig::Mssql {
                connection_string: Some("${MSSQL_CONNECTION_STRING}".into()),
            }),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        }
    }

    fn credentials_response(llm_api_key: &str) -> crate::api_client::CredentialsResponse {
        crate::api_client::CredentialsResponse {
            credentials: crate::api_client::StsCreds {
                access_key_id: "ak".into(),
                secret_access_key: "sk".into(),
                session_token: "st".into(),
                expiration: "2099-01-01T00:00:00Z".into(),
            },
            bucket: "skippr-prod".into(),
            tenant_id: "c3471188-8965-4c52-b486-7dbbd7a2d329".into(),
            llm_api_key: llm_api_key.into(),
            accounting_url: String::new(),
            knowledge_credentials: None,
            public_vectors_bucket: None,
        }
    }

    fn token_provider() -> std::sync::Arc<react_suite_data_engineer::metering::TokenProvider> {
        std::sync::Arc::new(react_suite_data_engineer::metering::TokenProvider::new(
            Some("token".to_string()),
            None,
            None,
        ))
    }

    #[test]
    fn translate_minimal_snowflake() {
        let cfg = SkipprDbtConfig {
            project: "my_project".into(),
            warehouse: Some(WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: Some("ANALYTICS".into()),
                schema: Some("RAW".into()),
                warehouse: Some("COMPUTE_WH".into()),
                role: Some("ACCOUNTADMIN".into()),
            }),
            source: Some(SourceConfig::Mssql {
                connection_string: Some("${MSSQL_CONNECTION_STRING}".into()),
            }),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };

        let internal = to_internal(&cfg).unwrap();
        assert_eq!(
            internal.scope.as_ref().unwrap().project_id.as_deref(),
            Some("my_project")
        );
        let p = internal.providers.unwrap();
        assert_eq!(p["warehouse"]["kind"], "snowflake");
        assert_eq!(p["el"]["enabled"], true);
        assert_eq!(p["dbt"]["naming"]["target_schema"], "my_project");
        assert_eq!(p["dbt"]["naming"]["silver_suffix"], "silver");
        assert_eq!(p["dbt"]["naming"]["gold_suffix"], "gold");
        assert_eq!(p["dbt"]["target"], "snowflake");
    }

    #[test]
    fn translate_missing_warehouse_errors() {
        let cfg = SkipprDbtConfig {
            project: "test".into(),
            warehouse: None,
            source: None,
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };
        assert!(to_internal(&cfg).is_err());
    }

    #[test]
    fn translate_postgres_warehouse() {
        let cfg = SkipprDbtConfig {
            project: "pg_project".into(),
            warehouse: Some(WarehouseConfig::Postgres {
                database: Some("analytics".into()),
                schema: Some("public".into()),
            }),
            source: Some(SourceConfig::Mssql {
                connection_string: Some("${MSSQL_CONNECTION_STRING}".into()),
            }),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };

        let internal = to_internal(&cfg).unwrap();
        assert_eq!(
            internal.scope.as_ref().unwrap().project_id.as_deref(),
            Some("pg_project")
        );
        let p = internal.providers.unwrap();
        assert_eq!(p["warehouse"]["kind"], "postgres");
        assert_eq!(p["warehouse"]["database"], "analytics");
        assert_eq!(p["dbt"]["target"], "postgres");
        assert_eq!(p["dbt"]["naming"]["target_schema"], "pg_project");
    }

    #[test]
    fn translate_empty_project_errors() {
        let cfg = SkipprDbtConfig {
            project: "".into(),
            warehouse: Some(WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            }),
            source: None,
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };
        assert!(to_internal(&cfg).is_err());
    }

    fn make_cfg(warehouse: WarehouseConfig, source: SourceConfig) -> SkipprDbtConfig {
        SkipprDbtConfig {
            project: "test_proj".into(),
            warehouse: Some(warehouse),
            source: Some(source),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        }
    }

    fn el_input(cfg: &SkipprDbtConfig) -> serde_json::Value {
        let internal = to_internal(cfg).unwrap();
        let p = internal.providers.unwrap();
        p["el"]["skippr_input"].clone()
    }

    fn wh_json(cfg: &SkipprDbtConfig) -> serde_json::Value {
        let internal = to_internal(cfg).unwrap();
        let p = internal.providers.unwrap();
        p["warehouse"].clone()
    }

    #[test]
    fn translate_mysql_source() {
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            },
            SourceConfig::Mysql {
                connection_string: Some("mysql://root@localhost".into()),
                tables: Some(vec!["users".into()]),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "mysql");
        assert_eq!(input["connection_string"], "mysql://root@localhost");
        assert_eq!(input["tables"][0], "users");
    }

    #[test]
    fn translate_postgres_source() {
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            },
            SourceConfig::PostgresSource {
                host: Some("db.example.com".into()),
                port: Some(5432),
                user: Some("pguser".into()),
                password: None,
                database: Some("mydb".into()),
                connection_string: None,
                tables: None,
                query: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "postgres");
        assert_eq!(input["host"], "db.example.com");
        assert_eq!(input["port"], 5432);
    }

    #[test]
    fn translate_kafka_source() {
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            },
            SourceConfig::Kafka {
                brokers: Some("localhost:9092".into()),
                topic: Some("events".into()),
                group_id: None,
                auto_offset_reset: None,
                security_protocol: None,
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password: None,
                mode: Some("batch".into()),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "kafka");
        assert_eq!(input["brokers"], "localhost:9092");
        assert_eq!(input["mode"], "batch");
    }

    #[test]
    fn translate_delta_lake_source() {
        let mut opts = std::collections::HashMap::new();
        opts.insert("AWS_REGION".to_string(), "us-east-1".to_string());
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            },
            SourceConfig::DeltaLake {
                table_uri: Some("s3://bucket/table".into()),
                storage_options: Some(opts),
                version: Some(5),
                filter: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "delta_lake");
        assert_eq!(input["table_uri"], "s3://bucket/table");
        assert_eq!(input["storage_options"]["AWS_REGION"], "us-east-1");
        assert_eq!(input["version"], 5);
    }

    #[test]
    fn translate_databricks_warehouse() {
        let cfg = make_cfg(
            WarehouseConfig::Databricks {
                workspace_url: Some("https://dbc-xxx.cloud.databricks.com".into()),
                token: Some("dapi123".into()),
                warehouse_id: Some("abc123".into()),
                catalog: Some("main".into()),
                schema: Some("default".into()),
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "databricks");
        assert_eq!(wh["workspace_url"], "https://dbc-xxx.cloud.databricks.com");
        assert_eq!(wh["catalog"], "main");
    }

    #[test]
    fn translate_redshift_warehouse() {
        let cfg = make_cfg(
            WarehouseConfig::Redshift {
                database: Some("analytics".into()),
                cluster_identifier: Some("my-cluster".into()),
                workgroup_name: None,
                db_user: Some("admin".into()),
                schema: Some("public".into()),
                region: Some("us-east-1".into()),
                staging_s3_bucket: Some("staging".into()),
                staging_s3_prefix: None,
                iam_role_arn: None,
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "redshift");
        assert_eq!(wh["database"], "analytics");
        assert_eq!(wh["cluster_identifier"], "my-cluster");
    }

    #[test]
    fn translate_clickhouse_warehouse() {
        let cfg = make_cfg(
            WarehouseConfig::Clickhouse {
                url: Some("http://ch:8123".into()),
                database: Some("default".into()),
                user: Some("default".into()),
                password: None,
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "clickhouse");
        assert_eq!(wh["url"], "http://ch:8123");
    }

    #[test]
    fn translate_motherduck_warehouse() {
        let cfg = make_cfg(
            WarehouseConfig::Motherduck {
                motherduck_token: Some("tok".into()),
                database: Some("my_db".into()),
                schema: Some("main".into()),
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "motherduck");
        assert_eq!(wh["motherduck_token"], "tok");
        assert_eq!(wh["database"], "my_db");
    }

    #[test]
    fn translate_synapse_warehouse() {
        let cfg = make_cfg(
            WarehouseConfig::Synapse {
                connection_string: Some("Server=tcp:myserver.database.windows.net".into()),
                schema: Some("dbo".into()),
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "synapse");
        assert_eq!(
            wh["connection_string"],
            "Server=tcp:myserver.database.windows.net"
        );
    }

    #[test]
    fn translate_snowflake_staging_fields() {
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: Some("acct".into()),
                user: Some("svc_user".into()),
                password: None,
                private_key_path: Some("/tmp/key.p8".into()),
                stage: Some("@skippr_stage".into()),
                staging_uri: Some("azure://acct.blob.core.windows.net/container/prefix".into()),
                staging_storage_integration: Some("SNOWFLAKE_AZURE_INT".into()),
                staging_azure_sas_token: Some("${AZURE_STORAGE_SAS_TOKEN}".into()),
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: Some("ANALYTICS".into()),
                schema: Some("RAW".into()),
                warehouse: Some("COMPUTE_WH".into()),
                role: Some("ACCOUNTADMIN".into()),
            },
            SourceConfig::Mssql {
                connection_string: None,
            },
        );
        let wh = wh_json(&cfg);
        assert_eq!(wh["kind"], "snowflake");
        assert_eq!(wh["stage"], "@skippr_stage");
        assert_eq!(
            wh["staging_uri"],
            "azure://acct.blob.core.windows.net/container/prefix"
        );
        assert_eq!(wh["staging_storage_integration"], "SNOWFLAKE_AZURE_INT");
        assert_eq!(wh["staging_azure_sas_token"], "${AZURE_STORAGE_SAS_TOKEN}");
    }

    #[test]
    fn translate_glue_schema_sink() {
        let cfg = SkipprDbtConfig {
            project: "test_proj".into(),
            warehouse: Some(WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            }),
            source: Some(SourceConfig::S3 {
                s3_bucket: Some("b".into()),
                s3_prefix: None,
                transform: None,
            }),
            dbt: None,
            schema_sink: Some(SchemaSinkConfig::Glue {
                glue_database_name: "my_glue_db".into(),
            }),
            ..Default::default()
        };
        let internal = to_internal(&cfg).unwrap();
        let p = internal.providers.unwrap();
        assert_eq!(p["el"]["schema_sink"]["kind"], "glue");
        assert_eq!(p["el"]["schema_sink"]["glue_database_name"], "my_glue_db");
    }

    #[test]
    fn translate_http_client_source() {
        let cfg = make_cfg(
            WarehouseConfig::Snowflake {
                account: None,
                user: None,
                password: None,
                private_key_path: None,
                stage: None,
                staging_uri: None,
                staging_storage_integration: None,
                staging_azure_sas_token: None,
                staging_azure_account_key: None,
                staging_gcs_service_account_key_path: None,
                database: None,
                schema: None,
                warehouse: None,
                role: None,
            },
            SourceConfig::HttpClient {
                url: Some("https://api.example.com/data".into()),
                method: Some("GET".into()),
                headers: None,
                body: None,
                auth_strategy: Some("bearer".into()),
                auth_user: None,
                auth_password: None,
                auth_token: Some("tok123".into()),
                scrape_interval_seconds: Some(60),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "http_client");
        assert_eq!(input["url"], "https://api.example.com/data");
        assert_eq!(input["auth"]["strategy"], "bearer");
        assert_eq!(input["auth"]["token"], "tok123");
        assert_eq!(input["scrape_interval_seconds"], 60);
    }

    #[test]
    fn authenticated_overlay_sets_scope_tenant_from_credentials() {
        with_clean_llm_env(|| {
            let mut internal = to_internal(&mssql_snowflake_config()).unwrap();
            let creds = credentials_response("server-llm-token");
            apply_authenticated_overlay(&mut internal, &creds, token_provider(), 0.0).unwrap();

            assert_eq!(
                internal.scope.as_ref().unwrap().tenant.as_deref(),
                Some("c3471188-8965-4c52-b486-7dbbd7a2d329")
            );
            assert_eq!(
                internal.llm.as_ref().unwrap().base_url.as_deref(),
                Some(DEFAULT_LLM_BASE_URL)
            );
            assert_eq!(
                std::env::var("LLM_API_KEY").ok().as_deref(),
                Some("server-llm-token")
            );
        });
    }

    #[test]
    fn authenticated_overlay_requires_server_llm_token_without_env_override() {
        with_clean_llm_env(|| {
            let mut internal = to_internal(&mssql_snowflake_config()).unwrap();
            let creds = credentials_response("");

            let err = apply_authenticated_overlay(&mut internal, &creds, token_provider(), 0.0)
                .expect_err("missing server LLM token should fail");
            assert!(err.contains("did not return an LLM token"));
        });
    }
}
