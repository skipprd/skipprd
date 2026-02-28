use crate::scope::RequestScope;
use serde::{Deserialize, Serialize};
use std::fmt;

// ── Enums that replace stringly-typed dispatch ──────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    Local,
    S3,
}

impl fmt::Display for StorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::S3 => write!(f, "s3"),
        }
    }
}

impl Default for StorageMode {
    fn default() -> Self {
        Self::Local
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarehouseKind {
    Athena,
    Postgres,
    Mssql,
    Snowflake,
    Bigquery,
}

impl fmt::Display for WarehouseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Athena => write!(f, "athena"),
            Self::Postgres => write!(f, "postgres"),
            Self::Mssql => write!(f, "mssql"),
            Self::Snowflake => write!(f, "snowflake"),
            Self::Bigquery => write!(f, "bigquery"),
        }
    }
}

impl Default for WarehouseKind {
    fn default() -> Self {
        Self::Athena
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LlmProvider {
    Null,
    OpenaiCompat,
    Openai,
    Http,
    LlamaCpp,
}

impl fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::OpenaiCompat => write!(f, "OPENAI_COMPAT"),
            Self::Openai => write!(f, "OPENAI"),
            Self::Http => write!(f, "HTTP"),
            Self::LlamaCpp => write!(f, "LLAMA_CPP"),
        }
    }
}

impl Default for LlmProvider {
    fn default() -> Self {
        Self::Null
    }
}

impl LlmProvider {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "OPENAI_COMPAT" => Self::OpenaiCompat,
            "OPENAI" => Self::Openai,
            "HTTP" => Self::Http,
            "LLAMA_CPP" => Self::LlamaCpp,
            "NULL" | "" => Self::Null,
            _ => Self::OpenaiCompat,
        }
    }
}

// ── Resolved config structs ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ReactResolvedConfig {
    pub server: ServerResolved,
    pub storage: StorageResolved,
    pub scope: RequestScope,
    pub llm: LlmResolved,
    pub providers: ProvidersResolved,
}

#[derive(Clone, Debug)]
pub struct ServerResolved {
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct StorageResolved {
    pub mode: StorageMode,
    pub bucket: Option<String>,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct LlmResolved {
    pub provider: LlmProvider,
    pub base_url: Option<String>,
    pub chat_model: Option<String>,
    pub embed_model: Option<String>,
    pub context_length: Option<usize>,
    pub gpu_layers: Option<usize>,
    pub http_timeout_secs: Option<u64>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct ProvidersResolved {
    pub warehouse: WarehouseResolved,
    pub catalog: CatalogResolved,
    pub dbt: DbtResolved,
    pub vector: VectorResolved,
}

#[derive(Clone, Debug)]
pub struct WarehouseResolved {
    pub kind: WarehouseKind,
    pub container: String,
    pub namespace: String,
    pub extras: serde_json::Value,
}

impl Default for WarehouseResolved {
    fn default() -> Self {
        Self {
            kind: WarehouseKind::default(),
            container: String::new(),
            namespace: String::new(),
            extras: serde_json::Value::Null,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CatalogResolved {
    pub enabled: bool,
    pub refresh_secs: u64,
    pub max_concurrency: usize,
}

#[derive(Clone, Debug, Default)]
pub struct VectorResolved {
    pub enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DbtNamingResolved {
    pub target_schema: String,
    pub silver_suffix: String,
    pub gold_suffix: String,
}

#[derive(Clone, Debug, Default)]
pub struct DbtResolved {
    pub enabled: bool,
    pub profiles_dir: Option<String>,
    pub target: String,
    pub naming: DbtNamingResolved,
    pub runner: String,
    pub docker_image: Option<String>,
    pub docker_platform: Option<String>,
    pub docker_network: Option<String>,
    pub docker_mount_aws_dir: bool,
}
