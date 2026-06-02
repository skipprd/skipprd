use react::config::{LlmFile, ReactConfigFile, ScopeFile, StorageFile};
use react_core::resolved_config::S3Credentials;

use crate::public_config::{SkipprProjectConfig, SourceConfig, WarehouseConfig};

const DEFAULT_LLM_BASE_URL: &str = "https://api.openai.com";

/// Translate the public `skippr` config into the internal runtime config.
pub fn to_internal(
    cfg: &SkipprProjectConfig,
    workspace: Option<&str>,
) -> Result<ReactConfigFile, String> {
    let project = cfg.project.trim();
    if project.is_empty() {
        return Err("project name is required in skippr.yaml".to_string());
    }
    let workspace = workspace
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("dev");

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
                SourceConfig::Mssql {
                    connection_string,
                    tables,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "mssql".into());
                    if let Some(cs) = connection_string {
                        m.insert("connection_string".into(), cs.clone().into());
                    }
                    if let Some(v) = tables {
                        m.insert("tables".into(), serde_json::to_value(v).unwrap_or_default());
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
                SourceConfig::GoogleAnalytics {
                    property_id,
                    start_date,
                    end_date,
                    lookback_days,
                    stream_profile,
                    keep_empty_rows,
                    processing_lag_days,
                    window_in_days,
                    access_token,
                    oauth_token_url,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_refresh_token,
                    service_account_json_path,
                    streams,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "google_analytics".into());
                    if let Some(v) = property_id {
                        m.insert("property_id".into(), v.clone().into());
                        m.insert("name".into(), format!("GA4 property {v}").into());
                    }
                    if let Some(v) = start_date {
                        m.insert("start_date".into(), v.clone().into());
                    }
                    if let Some(v) = end_date {
                        m.insert("end_date".into(), v.clone().into());
                    }
                    if let Some(v) = lookback_days {
                        m.insert("lookback_days".into(), (*v).into());
                    }
                    if let Some(v) = stream_profile {
                        m.insert("stream_profile".into(), v.clone().into());
                    }
                    if let Some(v) = keep_empty_rows {
                        m.insert("keep_empty_rows".into(), (*v).into());
                    }
                    if let Some(v) = processing_lag_days {
                        m.insert("processing_lag_days".into(), (*v).into());
                    }
                    if let Some(v) = window_in_days {
                        m.insert("window_in_days".into(), (*v).into());
                    }
                    if let Some(v) = access_token {
                        m.insert("access_token".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_token_url {
                        m.insert("oauth_token_url".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_id {
                        m.insert("oauth_client_id".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_secret {
                        m.insert("oauth_client_secret".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_refresh_token {
                        m.insert("oauth_refresh_token".into(), v.clone().into());
                    }
                    if let Some(v) = service_account_json_path {
                        m.insert("service_account_json_path".into(), v.clone().into());
                    }
                    if let Some(v) = streams {
                        m.insert(
                            "streams".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::GoogleSearchConsole {
                    site_url,
                    start_date,
                    end_date,
                    lookback_days,
                    stream_profile,
                    processing_lag_days,
                    window_in_days,
                    access_token,
                    oauth_token_url,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_refresh_token,
                    service_account_json_path,
                    streams,
                    search_type,
                    data_state,
                    row_limit,
                    url_inspection_enabled,
                    url_list,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "google_search_console".into());
                    if let Some(v) = site_url {
                        m.insert("site_url".into(), v.clone().into());
                        m.insert("name".into(), format!("GSC property {v}").into());
                    }
                    if let Some(v) = start_date {
                        m.insert("start_date".into(), v.clone().into());
                    }
                    if let Some(v) = end_date {
                        m.insert("end_date".into(), v.clone().into());
                    }
                    if let Some(v) = lookback_days {
                        m.insert("lookback_days".into(), (*v).into());
                    }
                    if let Some(v) = stream_profile {
                        m.insert("stream_profile".into(), v.clone().into());
                    }
                    if let Some(v) = processing_lag_days {
                        m.insert("processing_lag_days".into(), (*v).into());
                    }
                    if let Some(v) = window_in_days {
                        m.insert("window_in_days".into(), (*v).into());
                    }
                    if let Some(v) = access_token {
                        m.insert("access_token".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_token_url {
                        m.insert("oauth_token_url".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_id {
                        m.insert("oauth_client_id".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_secret {
                        m.insert("oauth_client_secret".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_refresh_token {
                        m.insert("oauth_refresh_token".into(), v.clone().into());
                    }
                    if let Some(v) = service_account_json_path {
                        m.insert("service_account_json_path".into(), v.clone().into());
                    }
                    if let Some(v) = streams {
                        m.insert(
                            "streams".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = search_type {
                        m.insert("search_type".into(), v.clone().into());
                    }
                    if let Some(v) = data_state {
                        m.insert("data_state".into(), v.clone().into());
                    }
                    if let Some(v) = row_limit {
                        m.insert("row_limit".into(), (*v).into());
                    }
                    if let Some(v) = url_inspection_enabled {
                        m.insert("url_inspection_enabled".into(), (*v).into());
                    }
                    if let Some(v) = url_list {
                        m.insert(
                            "url_list".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::BingWebmasterTools {
                    site_url,
                    api_key,
                    start_date,
                    end_date,
                    lookback_days,
                    stream_profile,
                    processing_lag_days,
                    window_in_days,
                    access_token,
                    oauth_token_url,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_refresh_token,
                    streams,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "bing_webmaster_tools".into());
                    if let Some(v) = site_url {
                        m.insert("site_url".into(), v.clone().into());
                        m.insert("name".into(), format!("Bing Webmaster {v}").into());
                    }
                    if let Some(v) = api_key {
                        m.insert("api_key".into(), v.clone().into());
                    }
                    if let Some(v) = start_date {
                        m.insert("start_date".into(), v.clone().into());
                    }
                    if let Some(v) = end_date {
                        m.insert("end_date".into(), v.clone().into());
                    }
                    if let Some(v) = lookback_days {
                        m.insert("lookback_days".into(), (*v).into());
                    }
                    if let Some(v) = stream_profile {
                        m.insert("stream_profile".into(), v.clone().into());
                    }
                    if let Some(v) = processing_lag_days {
                        m.insert("processing_lag_days".into(), (*v).into());
                    }
                    if let Some(v) = window_in_days {
                        m.insert("window_in_days".into(), (*v).into());
                    }
                    if let Some(v) = access_token {
                        m.insert("access_token".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_token_url {
                        m.insert("oauth_token_url".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_id {
                        m.insert("oauth_client_id".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_secret {
                        m.insert("oauth_client_secret".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_refresh_token {
                        m.insert("oauth_refresh_token".into(), v.clone().into());
                    }
                    if let Some(v) = streams {
                        m.insert(
                            "streams".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::GooglePageSpeed {
                    site,
                    api_key,
                    url_mode,
                    url_list,
                    max_urls,
                    strategies,
                    categories,
                    locale,
                    max_requests_per_run,
                    requests_per_minute,
                    respect_robots,
                    top_audits_per_page,
                    max_concurrent_requests,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "google_pagespeed".into());
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert("name".into(), format!("PageSpeed site {v}").into());
                    }
                    if let Some(v) = api_key {
                        m.insert("api_key".into(), v.clone().into());
                    }
                    if let Some(v) = url_mode {
                        m.insert("url_mode".into(), v.clone().into());
                    }
                    if let Some(v) = url_list {
                        m.insert(
                            "url_list".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = max_urls {
                        m.insert("max_urls".into(), (*v).into());
                    }
                    if let Some(v) = strategies {
                        m.insert(
                            "strategies".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = categories {
                        m.insert(
                            "categories".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = locale {
                        m.insert("locale".into(), v.clone().into());
                    }
                    if let Some(v) = max_requests_per_run {
                        m.insert("max_requests_per_run".into(), (*v).into());
                    }
                    if let Some(v) = requests_per_minute {
                        m.insert("requests_per_minute".into(), (*v).into());
                    }
                    if let Some(v) = respect_robots {
                        m.insert("respect_robots".into(), (*v).into());
                    }
                    if let Some(v) = top_audits_per_page {
                        m.insert("top_audits_per_page".into(), (*v).into());
                    }
                    if let Some(v) = max_concurrent_requests {
                        m.insert("max_concurrent_requests".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::SeoCrawl {
                    site,
                    max_urls,
                    max_depth,
                    crawl_rate_per_second,
                    respect_robots,
                    openai_enabled,
                    openai_model,
                    openai_analyze_blocks,
                    openai_max_blocks_per_page,
                    skip_unchanged_content,
                    user_agent,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "seo_crawl".into());
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert("name".into(), format!("SEO Crawl {v}").into());
                    }
                    if let Some(v) = max_urls {
                        m.insert("max_urls".into(), (*v).into());
                    }
                    if let Some(v) = max_depth {
                        m.insert("max_depth".into(), (*v).into());
                    }
                    if let Some(v) = crawl_rate_per_second {
                        m.insert("crawl_rate_per_second".into(), (*v).into());
                    }
                    if let Some(v) = respect_robots {
                        m.insert("respect_robots".into(), (*v).into());
                    }
                    if let Some(v) = openai_enabled {
                        m.insert("openai_enabled".into(), (*v).into());
                    }
                    if let Some(v) = openai_model {
                        m.insert("openai_model".into(), v.clone().into());
                    }
                    if let Some(v) = openai_analyze_blocks {
                        m.insert("openai_analyze_blocks".into(), (*v).into());
                    }
                    if let Some(v) = openai_max_blocks_per_page {
                        m.insert("openai_max_blocks_per_page".into(), (*v).into());
                    }
                    if let Some(v) = skip_unchanged_content {
                        m.insert("skip_unchanged_content".into(), (*v).into());
                    }
                    if let Some(v) = user_agent {
                        m.insert("user_agent".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::GoogleSerpRanks {
                    targets,
                    keywords,
                    country,
                    language,
                    device,
                    max_depth,
                    min_query_interval_ms,
                    max_queries_per_run,
                    stop_after_first_target_match,
                    capture_results,
                    force_refresh_today,
                    navigation_timeout_ms,
                    worker_node_path,
                    playwright_executable_path,
                    user_agent,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "google_serp_ranks".into());
                    if let Some(v) = targets {
                        m.insert(
                            "targets".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                        if let Some(first) = m
                            .get("targets")
                            .and_then(|t| t.as_array())
                            .and_then(|a| a.first())
                        {
                            if let Some(site) = first.get("site").and_then(|s| s.as_str()) {
                                m.insert("name".into(), format!("Google SERP {site}").into());
                            }
                        }
                    }
                    if let Some(v) = keywords {
                        m.insert(
                            "keywords".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = country {
                        m.insert("country".into(), v.clone().into());
                    }
                    if let Some(v) = language {
                        m.insert("language".into(), v.clone().into());
                    }
                    if let Some(v) = device {
                        m.insert("device".into(), v.clone().into());
                    }
                    if let Some(v) = max_depth {
                        m.insert("max_depth".into(), (*v).into());
                    }
                    if let Some(v) = min_query_interval_ms {
                        m.insert("min_query_interval_ms".into(), (*v).into());
                    }
                    if let Some(v) = max_queries_per_run {
                        m.insert("max_queries_per_run".into(), (*v).into());
                    }
                    if let Some(v) = stop_after_first_target_match {
                        m.insert("stop_after_first_target_match".into(), (*v).into());
                    }
                    if let Some(v) = capture_results {
                        m.insert("capture_results".into(), (*v).into());
                    }
                    if let Some(v) = force_refresh_today {
                        m.insert("force_refresh_today".into(), (*v).into());
                    }
                    if let Some(v) = navigation_timeout_ms {
                        m.insert("navigation_timeout_ms".into(), (*v).into());
                    }
                    if let Some(v) = worker_node_path {
                        m.insert("worker_node_path".into(), v.clone().into());
                    }
                    if let Some(v) = playwright_executable_path {
                        m.insert("playwright_executable_path".into(), v.clone().into());
                    }
                    if let Some(v) = user_agent {
                        m.insert("user_agent".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::AiCitations {
                    site,
                    brand_names,
                    prompt_list,
                    models,
                    requests_per_minute,
                    max_prompts_per_run,
                    skip_unchanged_responses,
                    openai_base_url,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "ai_citations".into());
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert("name".into(), format!("AI Citations {v}").into());
                    }
                    if let Some(v) = brand_names {
                        m.insert(
                            "brand_names".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = prompt_list {
                        m.insert(
                            "prompt_list".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = models {
                        m.insert("models".into(), serde_json::to_value(v).unwrap_or_default());
                    }
                    if let Some(v) = requests_per_minute {
                        m.insert("requests_per_minute".into(), (*v).into());
                    }
                    if let Some(v) = max_prompts_per_run {
                        m.insert("max_prompts_per_run".into(), (*v).into());
                    }
                    if let Some(v) = skip_unchanged_responses {
                        m.insert("skip_unchanged_responses".into(), (*v).into());
                    }
                    if let Some(v) = openai_base_url {
                        m.insert("openai_base_url".into(), v.clone().into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::SiteQuality {
                    site,
                    url_mode,
                    url_list,
                    max_pages_per_run,
                    wait_until,
                    navigation_timeout_ms,
                    lighthouse_enabled,
                    lighthouse_categories,
                    axe_enabled,
                    axe_tags,
                    pages_per_minute,
                    worker_node_path,
                    playwright_executable_path,
                    respect_robots,
                    skip_heavy_when_unchanged,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "site_quality".into());
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert("name".into(), format!("Site Quality {v}").into());
                    }
                    if let Some(v) = url_mode {
                        m.insert("url_mode".into(), v.clone().into());
                    }
                    if let Some(v) = url_list {
                        m.insert(
                            "url_list".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = max_pages_per_run {
                        m.insert("max_pages_per_run".into(), (*v).into());
                    }
                    if let Some(v) = wait_until {
                        m.insert("wait_until".into(), v.clone().into());
                    }
                    if let Some(v) = navigation_timeout_ms {
                        m.insert("navigation_timeout_ms".into(), (*v).into());
                    }
                    if let Some(v) = lighthouse_enabled {
                        m.insert("lighthouse_enabled".into(), (*v).into());
                    }
                    if let Some(v) = lighthouse_categories {
                        m.insert(
                            "lighthouse_categories".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = axe_enabled {
                        m.insert("axe_enabled".into(), (*v).into());
                    }
                    if let Some(v) = axe_tags {
                        m.insert(
                            "axe_tags".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = pages_per_minute {
                        m.insert("pages_per_minute".into(), (*v).into());
                    }
                    if let Some(v) = worker_node_path {
                        m.insert("worker_node_path".into(), v.clone().into());
                    }
                    if let Some(v) = playwright_executable_path {
                        m.insert("playwright_executable_path".into(), v.clone().into());
                    }
                    if let Some(v) = respect_robots {
                        m.insert("respect_robots".into(), (*v).into());
                    }
                    if let Some(v) = skip_heavy_when_unchanged {
                        m.insert("skip_heavy_when_unchanged".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::AppleSearchAds {
                    org_id,
                    client_id,
                    team_id,
                    key_id,
                    private_key_path,
                    private_key_pem,
                    start_date,
                    end_date,
                    lookback_days,
                    stream_profile,
                    processing_lag_days,
                    time_zone,
                    access_token,
                    streams,
                    return_records_with_no_metrics,
                    max_concurrent_requests,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "apple_search_ads".into());
                    if let Some(v) = org_id {
                        m.insert("org_id".into(), v.clone().into());
                        m.insert("name".into(), format!("Apple Search Ads org {v}").into());
                    }
                    if let Some(v) = client_id {
                        m.insert("client_id".into(), v.clone().into());
                    }
                    if let Some(v) = team_id {
                        m.insert("team_id".into(), v.clone().into());
                    }
                    if let Some(v) = key_id {
                        m.insert("key_id".into(), v.clone().into());
                    }
                    if let Some(v) = private_key_path {
                        m.insert("private_key_path".into(), v.clone().into());
                    }
                    if let Some(v) = private_key_pem {
                        m.insert("private_key_pem".into(), v.clone().into());
                    }
                    if let Some(v) = start_date {
                        m.insert("start_date".into(), v.clone().into());
                    }
                    if let Some(v) = end_date {
                        m.insert("end_date".into(), v.clone().into());
                    }
                    if let Some(v) = lookback_days {
                        m.insert("lookback_days".into(), (*v).into());
                    }
                    if let Some(v) = stream_profile {
                        m.insert("stream_profile".into(), v.clone().into());
                    }
                    if let Some(v) = processing_lag_days {
                        m.insert("processing_lag_days".into(), (*v).into());
                    }
                    if let Some(v) = time_zone {
                        m.insert("time_zone".into(), v.clone().into());
                    }
                    if let Some(v) = access_token {
                        m.insert("access_token".into(), v.clone().into());
                    }
                    if let Some(v) = streams {
                        m.insert(
                            "streams".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
                    }
                    if let Some(v) = return_records_with_no_metrics {
                        m.insert("return_records_with_no_metrics".into(), (*v).into());
                    }
                    if let Some(v) = max_concurrent_requests {
                        m.insert("max_concurrent_requests".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::DataForSeoBacklinks {
                    login,
                    password,
                    site,
                    run_mode,
                    backlink_target,
                    limit,
                    max_pages,
                    request_interval_ms,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "dataforseo_backlinks".into());
                    if let Some(v) = login {
                        m.insert("login".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert("name".into(), format!("DataForSEO Backlinks {v}").into());
                    }
                    if let Some(v) = run_mode {
                        m.insert("run_mode".into(), v.clone().into());
                    }
                    if let Some(v) = backlink_target {
                        m.insert("backlink_target".into(), v.clone().into());
                    }
                    if let Some(v) = limit {
                        m.insert("limit".into(), (*v).into());
                    }
                    if let Some(v) = max_pages {
                        m.insert("max_pages".into(), (*v).into());
                    }
                    if let Some(v) = request_interval_ms {
                        m.insert("request_interval_ms".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::DataForSeoSeoOpportunities {
                    login,
                    password,
                    site,
                    location_code,
                    language_code,
                    device,
                    run_mode,
                    seed_keywords,
                    request_interval_ms,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "dataforseo_seo_opportunities".into());
                    if let Some(v) = login {
                        m.insert("login".into(), v.clone().into());
                    }
                    if let Some(v) = password {
                        m.insert("password".into(), v.clone().into());
                    }
                    if let Some(v) = site {
                        m.insert("site".into(), v.clone().into());
                        m.insert(
                            "name".into(),
                            format!("DataForSEO Keyword Research {v}").into(),
                        );
                    }
                    if let Some(v) = location_code {
                        m.insert("location_code".into(), (*v).into());
                    }
                    if let Some(v) = language_code {
                        m.insert("language_code".into(), v.clone().into());
                    }
                    if let Some(v) = device {
                        m.insert("device".into(), v.clone().into());
                    }
                    if let Some(v) = run_mode {
                        m.insert("run_mode".into(), v.clone().into());
                    }
                    if let Some(v) = seed_keywords {
                        m.insert(
                            "seed_keywords".into(),
                            serde_json::Value::Array(
                                v.iter()
                                    .map(|s| serde_json::Value::String(s.clone()))
                                    .collect(),
                            ),
                        );
                    }
                    if let Some(v) = request_interval_ms {
                        m.insert("request_interval_ms".into(), (*v).into());
                    }
                    serde_json::Value::Object(m)
                }
                SourceConfig::MetaInstagramAds {
                    ad_account_id,
                    start_date,
                    end_date,
                    lookback_days,
                    stream_profile,
                    processing_lag_days,
                    api_version,
                    access_token,
                    oauth_token_url,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_refresh_token,
                    instagram_filter,
                    streams,
                } => {
                    let mut m = serde_json::Map::new();
                    m.insert("kind".into(), "meta_instagram_ads".into());
                    if let Some(v) = ad_account_id {
                        m.insert("ad_account_id".into(), v.clone().into());
                        m.insert("name".into(), format!("Meta ad account {v}").into());
                    }
                    if let Some(v) = start_date {
                        m.insert("start_date".into(), v.clone().into());
                    }
                    if let Some(v) = end_date {
                        m.insert("end_date".into(), v.clone().into());
                    }
                    if let Some(v) = lookback_days {
                        m.insert("lookback_days".into(), (*v).into());
                    }
                    if let Some(v) = stream_profile {
                        m.insert("stream_profile".into(), v.clone().into());
                    }
                    if let Some(v) = processing_lag_days {
                        m.insert("processing_lag_days".into(), (*v).into());
                    }
                    if let Some(v) = api_version {
                        m.insert("api_version".into(), v.clone().into());
                    }
                    if let Some(v) = access_token {
                        m.insert("access_token".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_token_url {
                        m.insert("oauth_token_url".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_id {
                        m.insert("oauth_client_id".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_client_secret {
                        m.insert("oauth_client_secret".into(), v.clone().into());
                    }
                    if let Some(v) = oauth_refresh_token {
                        m.insert("oauth_refresh_token".into(), v.clone().into());
                    }
                    if let Some(v) = instagram_filter {
                        m.insert("instagram_filter".into(), (*v).into());
                    }
                    if let Some(v) = streams {
                        m.insert(
                            "streams".into(),
                            serde_json::to_value(v).unwrap_or_default(),
                        );
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
            workspace: Some(workspace.to_string()),
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
#[allow(dead_code)]
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

    fn mssql_snowflake_config() -> SkipprProjectConfig {
        SkipprProjectConfig {
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
                tables: None,
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
        let cfg = SkipprProjectConfig {
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
                tables: None,
            }),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };

        let internal = to_internal(&cfg, None).unwrap();
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
        let cfg = SkipprProjectConfig {
            project: "test".into(),
            warehouse: None,
            source: None,
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };
        assert!(to_internal(&cfg, None).is_err());
    }

    #[test]
    fn translate_postgres_warehouse() {
        let cfg = SkipprProjectConfig {
            project: "pg_project".into(),
            warehouse: Some(WarehouseConfig::Postgres {
                database: Some("analytics".into()),
                schema: Some("public".into()),
            }),
            source: Some(SourceConfig::Mssql {
                connection_string: Some("${MSSQL_CONNECTION_STRING}".into()),
                tables: None,
            }),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        };

        let internal = to_internal(&cfg, None).unwrap();
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
        let cfg = SkipprProjectConfig {
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
        assert!(to_internal(&cfg, None).is_err());
    }

    fn make_cfg(warehouse: WarehouseConfig, source: SourceConfig) -> SkipprProjectConfig {
        SkipprProjectConfig {
            project: "test_proj".into(),
            warehouse: Some(warehouse),
            source: Some(source),
            dbt: None,
            schema_sink: None,
            ..Default::default()
        }
    }

    fn el_input(cfg: &SkipprProjectConfig) -> serde_json::Value {
        let internal = to_internal(cfg, None).unwrap();
        let p = internal.providers.unwrap();
        p["el"]["skippr_input"].clone()
    }

    fn wh_json(cfg: &SkipprProjectConfig) -> serde_json::Value {
        let internal = to_internal(cfg, None).unwrap();
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
                tables: None,
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
                tables: None,
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
                tables: None,
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
                tables: None,
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
                tables: None,
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
                tables: None,
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
        let cfg = SkipprProjectConfig {
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
        let internal = to_internal(&cfg, None).unwrap();
        let p = internal.providers.unwrap();
        assert_eq!(p["el"]["schema_sink"]["kind"], "glue");
        assert_eq!(p["el"]["schema_sink"]["glue_database_name"], "my_glue_db");
    }

    #[test]
    fn translate_google_search_console_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("seo".into()),
            },
            SourceConfig::GoogleSearchConsole {
                site_url: Some("https://example.com/".into()),
                start_date: Some("2024-01-01".into()),
                end_date: None,
                lookback_days: Some(3),
                stream_profile: Some("standard".into()),
                processing_lag_days: Some(3),
                window_in_days: Some(1),
                access_token: Some("${GSC_ACCESS_TOKEN}".into()),
                oauth_token_url: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                oauth_refresh_token: None,
                service_account_json_path: None,
                streams: Some(vec!["google_search_console.query_daily".into()]),
                search_type: Some("web".into()),
                data_state: Some("final".into()),
                row_limit: Some(25000),
                url_inspection_enabled: Some(false),
                url_list: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "google_search_console");
        assert_eq!(input["site_url"], "https://example.com/");
        assert_eq!(input["start_date"], "2024-01-01");
        assert_eq!(input["stream_profile"], "standard");
        assert_eq!(input["processing_lag_days"], 3);
        assert_eq!(input["access_token"], "${GSC_ACCESS_TOKEN}");
        assert_eq!(input["streams"][0], "google_search_console.query_daily");
    }

    #[test]
    fn translate_bing_webmaster_tools_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("seo".into()),
            },
            SourceConfig::BingWebmasterTools {
                site_url: Some("https://example.com/".into()),
                api_key: Some("${BING_WEBMASTER_TOOLS_API_KEY}".into()),
                start_date: Some("2026-03-01".into()),
                end_date: None,
                lookback_days: Some(3),
                stream_profile: Some("standard".into()),
                processing_lag_days: Some(3),
                window_in_days: Some(1),
                access_token: None,
                oauth_token_url: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                oauth_refresh_token: None,
                streams: Some(vec!["bing_webmaster_tools.query_daily".into()]),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "bing_webmaster_tools");
        assert_eq!(input["site_url"], "https://example.com/");
        assert_eq!(input["api_key"], "${BING_WEBMASTER_TOOLS_API_KEY}");
        assert_eq!(input["stream_profile"], "standard");
        assert_eq!(input["streams"][0], "bing_webmaster_tools.query_daily");
    }

    #[test]
    fn translate_google_analytics_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("analytics".into()),
            },
            SourceConfig::GoogleAnalytics {
                property_id: Some("123456789".into()),
                start_date: Some("2024-01-01".into()),
                end_date: None,
                lookback_days: Some(7),
                stream_profile: Some("minimal".into()),
                keep_empty_rows: Some(true),
                processing_lag_days: Some(1),
                window_in_days: Some(1),
                access_token: Some("${GA4_ACCESS_TOKEN}".into()),
                oauth_token_url: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                oauth_refresh_token: None,
                service_account_json_path: None,
                streams: Some(vec!["google_analytics.events_daily".into()]),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "google_analytics");
        assert_eq!(input["property_id"], "123456789");
        assert_eq!(input["start_date"], "2024-01-01");
        assert_eq!(input["lookback_days"], 7);
        assert_eq!(input["stream_profile"], "minimal");
        assert_eq!(input["keep_empty_rows"], true);
        assert_eq!(input["processing_lag_days"], 1);
        assert_eq!(input["window_in_days"], 1);
        assert_eq!(input["access_token"], "${GA4_ACCESS_TOKEN}");
        assert_eq!(input["streams"][0], "google_analytics.events_daily");
    }

    #[test]
    fn translate_google_pagespeed_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("web".into()),
            },
            SourceConfig::GooglePageSpeed {
                site: Some("https://example.com".into()),
                api_key: Some("${PAGESPEED_API_KEY}".into()),
                url_mode: Some("tld_sample".into()),
                url_list: None,
                max_urls: Some(50),
                strategies: Some(vec!["mobile".into(), "desktop".into()]),
                categories: Some(vec!["performance".into(), "seo".into()]),
                locale: Some("en_US".into()),
                max_requests_per_run: Some(120),
                requests_per_minute: Some(30),
                respect_robots: Some(true),
                top_audits_per_page: Some(15),
                max_concurrent_requests: Some(2),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "google_pagespeed");
        assert_eq!(input["site"], "https://example.com");
        assert_eq!(input["api_key"], "${PAGESPEED_API_KEY}");
        assert_eq!(input["url_mode"], "tld_sample");
        assert_eq!(input["max_urls"], 50);
        assert_eq!(input["strategies"][0], "mobile");
        assert_eq!(input["max_requests_per_run"], 120);
    }

    #[test]
    fn translate_google_serp_ranks_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("seo".into()),
            },
            SourceConfig::GoogleSerpRanks {
                targets: Some(vec![crate::public_config::GoogleSerpTargetConfig {
                    site: "example.com".into(),
                    aliases: vec!["www.example.com".into()],
                }]),
                keywords: Some(vec!["best widgets".into()]),
                country: Some("uk".into()),
                language: Some("en".into()),
                device: Some("desktop".into()),
                max_depth: Some(30),
                min_query_interval_ms: Some(30_000),
                max_queries_per_run: Some(10),
                stop_after_first_target_match: Some(true),
                capture_results: Some(false),
                force_refresh_today: Some(false),
                navigation_timeout_ms: Some(45_000),
                worker_node_path: Some("node".into()),
                playwright_executable_path: None,
                user_agent: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "google_serp_ranks");
        assert_eq!(input["country"], "uk");
        assert_eq!(input["max_depth"], 30);
        assert_eq!(input["keywords"][0], "best widgets");
    }

    #[test]
    fn translate_ai_citations_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("marketing".into()),
            },
            SourceConfig::AiCitations {
                site: Some("https://example.com".into()),
                brand_names: Some(vec!["Example".into()]),
                prompt_list: Some(vec![TrackedPromptEntry {
                    id: "best_tools".into(),
                    text: "What are the best tools?".into(),
                    category: Some("discovery".into()),
                    intent: None,
                }]),
                models: Some(vec!["gpt-4.1-mini".into()]),
                requests_per_minute: Some(10),
                max_prompts_per_run: Some(50),
                skip_unchanged_responses: Some(true),
                openai_base_url: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "ai_citations");
        assert_eq!(input["site"], "https://example.com");
        assert_eq!(input["models"][0], "gpt-4.1-mini");
        assert_eq!(input["prompt_list"][0]["id"], "best_tools");
    }

    #[test]
    fn translate_site_quality_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("web".into()),
            },
            SourceConfig::SiteQuality {
                site: Some("https://example.com".into()),
                url_mode: Some("tld_sample".into()),
                url_list: None,
                max_pages_per_run: Some(50),
                wait_until: Some("networkidle".into()),
                navigation_timeout_ms: Some(45000),
                lighthouse_enabled: Some(true),
                lighthouse_categories: Some(vec!["performance".into()]),
                axe_enabled: Some(true),
                axe_tags: Some(vec!["wcag2aa".into()]),
                pages_per_minute: Some(6),
                worker_node_path: Some("node".into()),
                playwright_executable_path: None,
                respect_robots: Some(true),
                skip_heavy_when_unchanged: Some(true),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "site_quality");
        assert_eq!(input["site"], "https://example.com");
        assert_eq!(input["url_mode"], "tld_sample");
        assert_eq!(input["max_pages_per_run"], 50);
        assert_eq!(input["lighthouse_enabled"], true);
    }

    #[test]
    fn translate_apple_search_ads_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("marketing".into()),
            },
            SourceConfig::AppleSearchAds {
                org_id: Some("12345".into()),
                client_id: Some("client".into()),
                team_id: Some("team".into()),
                key_id: Some("key".into()),
                private_key_path: Some("${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}".into()),
                private_key_pem: None,
                start_date: Some("2024-01-01".into()),
                end_date: None,
                lookback_days: Some(3),
                stream_profile: Some("full".into()),
                processing_lag_days: Some(1),
                time_zone: Some("UTC".into()),
                access_token: None,
                streams: None,
                return_records_with_no_metrics: Some(true),
                max_concurrent_requests: Some(8),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "apple_search_ads");
        assert_eq!(input["org_id"], "12345");
        assert_eq!(input["start_date"], "2024-01-01");
        assert_eq!(input["stream_profile"], "full");
        assert_eq!(input["max_concurrent_requests"], 8);
    }

    #[test]
    fn translate_dataforseo_backlinks_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("marketing".into()),
            },
            SourceConfig::DataForSeoBacklinks {
                login: Some("${DATAFORSEO_LOGIN}".into()),
                password: Some("${DATAFORSEO_PASSWORD}".into()),
                site: Some("example.com".into()),
                run_mode: Some("both".into()),
                backlink_target: Some("example.com".into()),
                limit: Some(1000),
                max_pages: Some(5),
                request_interval_ms: Some(200),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "dataforseo_backlinks");
        assert_eq!(input["site"], "example.com");
        assert_eq!(input["backlink_target"], "example.com");
        assert_eq!(input["max_pages"], 5);
    }

    #[test]
    fn translate_dataforseo_seo_opportunities_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("marketing".into()),
            },
            SourceConfig::DataForSeoSeoOpportunities {
                login: Some("${DATAFORSEO_API_USER}".into()),
                password: Some("${DATAFORSEO_API_PASS}".into()),
                site: Some("example.com".into()),
                location_code: Some(2840),
                language_code: Some("en".into()),
                device: Some("desktop".into()),
                run_mode: Some("mvp".into()),
                seed_keywords: Some(vec!["meal planning app".into()]),
                request_interval_ms: Some(200),
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "dataforseo_seo_opportunities");
        assert_eq!(input["site"], "example.com");
        assert_eq!(input["seed_keywords"][0], "meal planning app");
    }

    #[test]
    fn translate_meta_instagram_ads_source() {
        let cfg = make_cfg(
            WarehouseConfig::Athena {
                workgroup: None,
                region: Some("us-east-1".into()),
                result_s3: None,
                schema: Some("marketing".into()),
            },
            SourceConfig::MetaInstagramAds {
                ad_account_id: Some("123456789".into()),
                start_date: Some("2024-01-01".into()),
                end_date: None,
                lookback_days: Some(3),
                stream_profile: Some("full".into()),
                processing_lag_days: Some(1),
                api_version: Some("v21.0".into()),
                access_token: Some("${META_INSTAGRAM_ADS_ACCESS_TOKEN}".into()),
                oauth_token_url: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                oauth_refresh_token: None,
                instagram_filter: Some(true),
                streams: None,
            },
        );
        let input = el_input(&cfg);
        assert_eq!(input["kind"], "meta_instagram_ads");
        assert_eq!(input["ad_account_id"], "123456789");
        assert_eq!(input["start_date"], "2024-01-01");
        assert_eq!(input["stream_profile"], "full");
        assert_eq!(input["instagram_filter"], true);
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
            let mut internal = to_internal(&mssql_snowflake_config(), None).unwrap();
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
            let mut internal = to_internal(&mssql_snowflake_config(), None).unwrap();
            let creds = credentials_response("");

            let err = apply_authenticated_overlay(&mut internal, &creds, token_provider(), 0.0)
                .expect_err("missing server LLM token should fail");
            assert!(err.contains("did not return an LLM token"));
        });
    }
}
