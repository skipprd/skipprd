use crate::scope::RequestScope;

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
    pub bucket: String,
}

#[derive(Clone, Debug, Default)]
pub struct LlmResolved {
    pub provider: Option<String>,
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

#[derive(Clone, Debug, Default)]
pub struct WarehouseResolved {
    pub kind: String,
    pub container: String,
    pub namespace: String,
    pub extras: serde_json::Value,
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
