use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Public `skippr` config schema.
///
/// This is the only config surface exposed to product users.
/// It maps to the internal runtime config shape silently.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprDbtConfig {
    pub project: String,

    #[serde(default)]
    pub warehouse: Option<WarehouseConfig>,

    #[serde(default)]
    pub source: Option<SourceConfig>,

    #[serde(default)]
    pub dbt: Option<DbtConfig>,

    #[serde(default)]
    pub schema_sink: Option<SchemaSinkConfig>,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    Mssql {
        #[serde(default)]
        connection_string: Option<String>,
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

impl SkipprDbtConfig {
    pub fn load_from(path: &Path) -> Result<Self, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        serde_yaml::from_slice::<Self>(&bytes)
            .map_err(|e| format!("failed to parse {}: {}", path.display(), e))
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
            Some(SourceConfig::HttpClient { .. }) => Some("http_client"),
            Some(SourceConfig::HttpServer { .. }) => Some("http_server"),
            Some(SourceConfig::Socket { .. }) => Some("socket"),
            Some(SourceConfig::Statsd { .. }) => Some("statsd"),
            Some(SourceConfig::Stdin { .. }) => Some("stdin"),
            None => None,
        }
    }

    pub fn uses_postgres(&self) -> bool {
        matches!(&self.warehouse, Some(WarehouseConfig::Postgres { .. }))
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
