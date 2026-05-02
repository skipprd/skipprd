use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_redshiftdata::Client as RedshiftClient;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceRedshiftPluginConfig {
    pub cluster_identifier: Option<String>,
    pub workgroup_name: Option<String>,
    pub database: String,
    pub db_user: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub region: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceRedshiftPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Redshift")
    }
}

pub struct DataSourceRedshiftPlugin {
    ingest: Ingest,
    config: DataSourceRedshiftPluginConfig,
    client: RedshiftClient,
}

impl DataSourceRedshiftPlugin {
    async fn from_config(config: DataSourceRedshiftPluginConfig) -> Self {
        let mut aws_builder = aws_config::defaults(BehaviorVersion::latest());
        if let Some(ref region) = config.region {
            aws_builder = aws_builder.region(aws_config::Region::new(region.clone()));
        }
        let aws_config = aws_builder.load().await;
        let client = RedshiftClient::new(&aws_config);

        Self {
            ingest: Ingest::new(),
            config,
            client,
        }
    }

    pub async fn new() -> Self {
        let config: DataSourceRedshiftPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
                Err(_) => DataSourceRedshiftPluginConfig {
                    cluster_identifier: Some(Config::getenv("REDSHIFT_CLUSTER_IDENTIFIER", "")),
                    workgroup_name: None,
                    database: Config::getenv("REDSHIFT_DATABASE", ""),
                    db_user: None,
                    tables: None,
                    query: None,
                    region: None,
                    format: None,
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                },
            };

        Self::from_config(config).await
    }

    pub async fn with_runtime_config(config: DataSourceRedshiftPluginConfig) -> Self {
        Self::from_config(config).await
    }

    async fn execute_and_fetch(&self, sql: &str) -> Result<Vec<String>, std::io::Error> {
        let mut stmt = self
            .client
            .execute_statement()
            .database(&self.config.database)
            .sql(sql);
        if let Some(ref cluster) = self.config.cluster_identifier {
            stmt = stmt.cluster_identifier(cluster);
        }
        if let Some(ref wg) = self.config.workgroup_name {
            stmt = stmt.workgroup_name(wg);
        }
        if let Some(ref user) = self.config.db_user {
            stmt = stmt.db_user(user);
        }

        let result = stmt
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let statement_id = result.id().unwrap_or_default().to_string();

        loop {
            let desc = self
                .client
                .describe_statement()
                .id(&statement_id)
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let status = desc
                .status()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            match status.as_str() {
                "FINISHED" => break,
                "FAILED" | "ABORTED" => {
                    return Err(std::io::Error::other(format!(
                        "Redshift statement {}: {}",
                        status,
                        desc.error().unwrap_or_default()
                    )));
                }
                _ => sleep(Duration::from_secs(1)).await,
            }
        }

        let mut rows = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut req = self.client.get_statement_result().id(&statement_id);
            if let Some(ref token) = next_token {
                req = req.next_token(token);
            }
            let page = req
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let columns: Vec<String> = page
                .column_metadata()
                .iter()
                .map(|c| c.name().unwrap_or("col").to_string())
                .collect();

            for record in page.records() {
                let mut map = serde_json::Map::new();
                for (i, field) in record.iter().enumerate() {
                    let col_name = columns
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("col_{}", i));
                    use aws_sdk_redshiftdata::types::Field;
                    let val = match field {
                        Field::IsNull(true) => serde_json::Value::Null,
                        Field::StringValue(s) => serde_json::Value::String(s.clone()),
                        Field::LongValue(l) => serde_json::Value::Number((*l).into()),
                        Field::DoubleValue(d) => serde_json::json!(d),
                        Field::BooleanValue(b) => serde_json::Value::Bool(*b),
                        Field::BlobValue(b) => serde_json::Value::String(hex::encode(b.as_ref())),
                        _ => serde_json::Value::Null,
                    };
                    map.insert(col_name, val);
                }
                rows.push(serde_json::to_string(&map).unwrap_or_default());
            }

            next_token = page.next_token().map(|s| s.to_string());
            if next_token.is_none() {
                break;
            }
        }

        Ok(rows)
    }
}

#[async_trait]
impl DataSource for DataSourceRedshiftPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let queries: Vec<(String, String)> = if let Some(ref q) = self.config.query {
            vec![("query".to_string(), q.clone())]
        } else if let Some(ref tables) = self.config.tables {
            tables
                .iter()
                .map(|t| (t.clone(), format!("SELECT * FROM {}", t)))
                .collect()
        } else {
            return Err(std::io::Error::other(
                "Redshift: must specify either 'tables' or 'query'",
            ));
        };

        for (table_name, query) in queries {
            let namespace = format!("redshift.{}.{}", self.config.database, table_name);
            info!("Redshift: executing query for {}", table_name);

            let rows = self.execute_and_fetch(&query).await?;

            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: table_name.clone(),
            };

            let batches: Vec<IngestBatch> = rows
                .into_iter()
                .map(|json_str| {
                    let bytes = json_str.len();
                    IngestBatch {
                        offset_key: offset_key.clone(),
                        data: json_str,
                        bytes,
                        source_uri: format!("redshift://{}/{}", self.config.database, table_name),
                        namespace: Some(namespace.clone()),
                        cdc_rows: None,
                    }
                })
                .collect();

            if !batches.is_empty() {
                let mut ingest_tasks = IngestTasks::new();
                ingest_tasks.add(IngestTask::new(
                    batches,
                    offsets.clone(),
                    shared_output.clone(),
                ));
                self.ingest
                    .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
            }
        }

        Ok(())
    }

    fn execution_contract(&self) -> crate::plugins::SourceExecutionContract {
        crate::plugins::SourceExecutionContract::finite()
    }
}
