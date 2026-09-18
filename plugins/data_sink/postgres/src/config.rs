use skippr_runtime_sdk::SkipprConfig;
use serde_derive::{Deserialize, Serialize};

/// Runtime / pipeline config for the Postgres data sink.
#[derive(Debug, Deserialize, SkipprConfig, Serialize, Clone)]
pub struct DataSinkPostgresPluginConfig {
    #[serde(default = "default_postgres_host")]
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    #[serde(default)]
    #[skippr(secret)]
    pub password: Option<String>,
    pub database: String,
    #[serde(default = "default_postgres_schema")]
    pub schema: String,
    pub sslmode: Option<String>,
    pub format: Option<String>,
}

fn default_postgres_host() -> String {
    "localhost".to_string()
}

fn default_postgres_schema() -> String {
    "public".to_string()
}
