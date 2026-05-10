use std::sync::Arc;

use serde_derive::Deserialize;
use tiberius::{numeric::Numeric, Client, ColumnData, Config as TiberiusConfig, Row};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use skippr_runtime_sdk::progress::{OffsetKey, OffsetTypes, Offsets};
use skippr_runtime_sdk::source_compat::{Ingest, IngestBatch, IngestTask, IngestTasks};
use skippr_runtime_sdk::plugins::{DataSink, DataSource};
use async_trait::async_trait;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMssqlPluginConfig {
    pub connection_string: String,
    pub tables: Option<Vec<String>>,
    pub batch_size_rows: Option<usize>,
    pub query_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<DataSourcePluginConfig> for DataSourceMssqlPluginConfig {
    type Error = String;

    fn try_from(plugin_config: DataSourcePluginConfig) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Mssql")
    }
}

pub struct DataSourceMssqlPlugin {
    pub(crate) ingest: Ingest,
    pub(crate) config: DataSourceMssqlPluginConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MssqlColumnSchema {
    name: String,
    data_type: String,
}

impl DataSourceMssqlPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMssqlPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(input_config) => input_config.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => {
                let connection_string = Config::getenv("MSSQL_CONNECTION_STRING", "");
                DataSourceMssqlPluginConfig {
                    connection_string,
                    tables: None,
                    batch_size_rows: None,
                    query_timeout_seconds: None,
                    format: Some("row".to_string()),
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                }
            }
        };

        DataSourceMssqlPlugin {
            ingest: Ingest::new(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceMssqlPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    async fn connect(
        config: &DataSourceMssqlPluginConfig,
    ) -> Result<Client<tokio_util::compat::Compat<TcpStream>>, Box<dyn std::error::Error>> {
        let tib_config = TiberiusConfig::from_ado_string(&config.connection_string)?;

        let tcp = TcpStream::connect(tib_config.get_addr()).await?;
        tcp.set_nodelay(true)?;

        let client = Client::connect(tib_config, tcp.compat_write()).await?;
        Ok(client)
    }

    async fn discover_tables(
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let sql = "SELECT TABLE_SCHEMA, TABLE_NAME \
                    FROM INFORMATION_SCHEMA.TABLES \
                    WHERE TABLE_TYPE = 'BASE TABLE' \
                    ORDER BY TABLE_SCHEMA, TABLE_NAME";
        let stream = client.simple_query(sql).await?;
        let rows: Vec<Row> = stream.into_first_result().await?;
        let mut tables = Vec::new();
        for row in rows {
            let schema: &str = row.get(0).unwrap_or("dbo");
            let table: &str = row.get(1).unwrap_or("");
            if !table.is_empty() {
                tables.push(format!("{}.{}", schema, table));
            }
        }
        Ok(tables)
    }

    async fn get_database_name(
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
    ) -> String {
        match client.simple_query("SELECT DB_NAME()").await {
            Ok(stream) => match stream.into_first_result().await {
                Ok(rows) => {
                    if let Some(row) = rows.first() {
                        row.get::<&str, _>(0).unwrap_or("unknown").to_string()
                    } else {
                        "unknown".to_string()
                    }
                }
                Err(_) => "unknown".to_string(),
            },
            Err(_) => "unknown".to_string(),
        }
    }

    fn sql_string_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    async fn table_columns(
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
        schema: &str,
        table: &str,
    ) -> Result<Vec<MssqlColumnSchema>, Box<dyn std::error::Error>> {
        let sql = format!(
            "SELECT COLUMN_NAME, DATA_TYPE \
             FROM INFORMATION_SCHEMA.COLUMNS \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} \
             ORDER BY ORDINAL_POSITION",
            Self::sql_string_literal(schema),
            Self::sql_string_literal(table)
        );
        let stream = client.simple_query(sql).await?;
        let rows: Vec<Row> = stream.into_first_result().await?;
        let mut columns = Vec::new();
        for row in rows {
            let name: &str = row.get(0).unwrap_or("");
            let data_type: &str = row.get(1).unwrap_or("");
            if !name.is_empty() && !data_type.is_empty() {
                columns.push(MssqlColumnSchema {
                    name: name.to_string(),
                    data_type: data_type.to_ascii_lowercase(),
                });
            }
        }
        Ok(columns)
    }

    fn seed_table_metadata(namespace: &str, columns: &[MssqlColumnSchema]) {
        let _ = (namespace, columns);
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = serde_json::Map::new();
        for (col, data) in row.cells() {
            let name = col.name().to_string();
            let value = Self::column_value_to_json(data);
            map.insert(name, value);
        }
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
    }

    fn numeric_to_json(n: Numeric) -> serde_json::Value {
        serde_json::Value::String(n.to_string())
    }

    fn naive_datetime_to_json_value(datetime: chrono::NaiveDateTime) -> serde_json::Value {
        serde_json::Value::String(datetime.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
    }

    fn tds_date_to_chrono(date: tiberius::time::Date) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap()
            + chrono::Duration::days(date.days() as i64)
    }

    fn tds_time_to_chrono(time: tiberius::time::Time) -> chrono::NaiveTime {
        let nanos_per_increment = 10_i64.pow(9 - u32::from(time.scale()));
        chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            + chrono::Duration::nanoseconds(time.increments() as i64 * nanos_per_increment)
    }

    fn tds_datetime2_to_chrono(datetime: tiberius::time::DateTime2) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::new(
            Self::tds_date_to_chrono(datetime.date()),
            Self::tds_time_to_chrono(datetime.time()),
        )
    }

    fn tds_legacy_datetime_to_chrono(days: i64, seconds_fragments: i64) -> chrono::NaiveDateTime {
        let date =
            chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap() + chrono::Duration::days(days);
        let time = chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            + chrono::Duration::nanoseconds(seconds_fragments * 1_000_000_000 / 300);
        chrono::NaiveDateTime::new(date, time)
    }

    fn tds_smalldatetime_to_chrono(days: i64, minute_fragments: i64) -> chrono::NaiveDateTime {
        let date =
            chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap() + chrono::Duration::days(days);
        let time = chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            + chrono::Duration::minutes(minute_fragments);
        chrono::NaiveDateTime::new(date, time)
    }

    fn column_value_to_json(data: &ColumnData<'static>) -> serde_json::Value {
        match data {
            ColumnData::U8(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::I16(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::I32(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::I64(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::F32(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::F64(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::Bit(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            ColumnData::String(v) => v
                .as_ref()
                .map_or(serde_json::Value::Null, |v| serde_json::json!(v.as_ref())),
            ColumnData::Guid(v) => v.map_or(serde_json::Value::Null, |v| {
                serde_json::Value::String(v.to_string())
            }),
            ColumnData::Binary(v) => v
                .as_ref()
                .map_or(serde_json::Value::Null, |v| serde_json::json!(v.as_ref())),
            ColumnData::Numeric(v) => v.map_or(serde_json::Value::Null, Self::numeric_to_json),
            ColumnData::Xml(v) => v.as_ref().map_or(serde_json::Value::Null, |v| {
                serde_json::Value::String(v.as_ref().to_string())
            }),
            ColumnData::DateTime(v) => v.map_or(serde_json::Value::Null, |v| {
                Self::naive_datetime_to_json_value(Self::tds_legacy_datetime_to_chrono(
                    i64::from(v.days()),
                    i64::from(v.seconds_fragments()),
                ))
            }),
            ColumnData::SmallDateTime(v) => v.map_or(serde_json::Value::Null, |v| {
                Self::naive_datetime_to_json_value(Self::tds_smalldatetime_to_chrono(
                    i64::from(v.days()),
                    i64::from(v.seconds_fragments()),
                ))
            }),
            ColumnData::Time(v) => v.map_or(serde_json::Value::Null, |v| {
                serde_json::Value::String(Self::tds_time_to_chrono(v).to_string())
            }),
            ColumnData::Date(v) => v.map_or(serde_json::Value::Null, |v| {
                serde_json::Value::String(Self::tds_date_to_chrono(v).to_string())
            }),
            ColumnData::DateTime2(v) => v.map_or(serde_json::Value::Null, |v| {
                Self::naive_datetime_to_json_value(Self::tds_datetime2_to_chrono(v))
            }),
            ColumnData::DateTimeOffset(v) => v.map_or(serde_json::Value::Null, |v| {
                let offset = chrono::FixedOffset::east_opt(i32::from(v.offset()) * 60).unwrap();
                let datetime = Self::tds_datetime2_to_chrono(v.datetime2());
                serde_json::Value::String(
                    chrono::DateTime::<chrono::FixedOffset>::from_naive_utc_and_offset(
                        datetime, offset,
                    )
                    .to_rfc3339(),
                )
            }),
        }
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        info!("MSSQL input plugin starting sync");

        let mut client = match Self::connect(&self.config).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to connect to MSSQL: {}", e);
                return;
            }
        };

        let db_name = Self::get_database_name(&mut client).await;

        let tables = match &self.config.tables {
            Some(t) => t.clone(),
            None => match Self::discover_tables(&mut client).await {
                Ok(t) => {
                    info!("Discovered {} tables from MSSQL", t.len());
                    t
                }
                Err(e) => {
                    error!("Failed to discover MSSQL tables: {}", e);
                    return;
                }
            },
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);

        for table_fq in &tables {
            let parts: Vec<&str> = table_fq.splitn(2, '.').collect();
            let (schema, table) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                ("dbo", parts[0])
            };

            let namespace = format!("mssql.{}.{}.{}", db_name, schema, table);
            let offset_key = OffsetKey {
                namespace: format!("mssql:{}.{}.{}", db_name, schema, table),
                partition: table_fq.clone(),
            };

            if offsets.validate(&offset_key, OffsetTypes::Closed, 1) == Some(true) {
                info!("Skipping already-ingested table: {}", table_fq);
                continue;
            }

            info!("Ingesting table: {} -> namespace: {}", table_fq, namespace);

            let query_sql = format!("SELECT * FROM [{}].[{}]", schema, table);
            match Self::table_columns(&mut client, schema, table).await {
                Ok(columns) => Self::seed_table_metadata(table, &columns),
                Err(e) => error!("Failed to read schema for {}: {}", table_fq, e),
            }

            let stream = match client.simple_query(&query_sql).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to query table {}: {}", table_fq, e);
                    continue;
                }
            };

            let rows: Vec<Row> = match stream.into_first_result().await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to read results for {}: {}", table_fq, e);
                    continue;
                }
            };

            info!("Read {} rows from {}", rows.len(), table_fq);

            let mut current_batch: Vec<IngestBatch> = Vec::new();
            let mut ingest_tasks = IngestTasks::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();

                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("mssql://{}/{}", db_name, table_fq),
                    namespace: Some(table.to_string()),
                    cdc_rows: None,
                });

                if current_batch.len() >= batch_size {
                    let batch = std::mem::take(&mut current_batch);
                    ingest_tasks.add(IngestTask::new(
                        batch,
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                }
            }

            if !current_batch.is_empty() {
                ingest_tasks.add(IngestTask::new(
                    current_batch,
                    offsets.clone(),
                    shared_output.clone(),
                ));
            }

            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
        }

        info!("MSSQL input plugin sync complete");
    }
}

#[async_trait]
impl DataSource for DataSourceMssqlPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tds_days_since_year_one(date: chrono::NaiveDate) -> u32 {
        date.signed_duration_since(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
            .num_days() as u32
    }

    #[test]
    fn numeric_to_json_preserves_decimal_value() {
        let value = DataSourceMssqlPlugin::numeric_to_json(Numeric::new_with_scale(12050, 2));

        assert_eq!(value, serde_json::json!("120.50"));
    }

    #[test]
    fn column_value_to_json_preserves_datetime2_value() {
        let date = chrono::NaiveDate::from_ymd_opt(2025, 1, 3).unwrap();
        let time_increments = (9 * 60 * 60 + 15 * 60) * 10_000_000;
        let datetime = tiberius::time::DateTime2::new(
            tiberius::time::Date::new(tds_days_since_year_one(date)),
            tiberius::time::Time::new(time_increments, 7),
        );

        let value =
            DataSourceMssqlPlugin::column_value_to_json(&ColumnData::DateTime2(Some(datetime)));

        assert_eq!(value, serde_json::json!("2025-01-03T09:15:00"));
    }

    #[test]
    fn column_value_to_json_preserves_decimal_value() {
        let value = DataSourceMssqlPlugin::column_value_to_json(&ColumnData::Numeric(Some(
            Numeric::new_with_scale(12050, 2),
        )));

        assert_eq!(value, serde_json::json!("120.50"));
    }

    #[test]
    fn maps_mssql_numeric_columns_to_decimal_metadata() {
        assert_eq!(
            DataSourceMssqlPlugin::skippr_type_for_mssql_type("decimal"),
            SkipprDataType::Decimal
        );
        assert_eq!(
            DataSourceMssqlPlugin::skippr_type_for_mssql_type("numeric"),
            SkipprDataType::Decimal
        );
    }

    #[test]
    fn builds_table_metadata_from_mssql_column_schema() {
        let metadata = DataSourceMssqlPlugin::metadata_for_columns(&[
            MssqlColumnSchema {
                name: "order_id".to_string(),
                data_type: "nvarchar".to_string(),
            },
            MssqlColumnSchema {
                name: "total_amount".to_string(),
                data_type: "decimal".to_string(),
            },
            MssqlColumnSchema {
                name: "placed_at".to_string(),
                data_type: "datetime2".to_string(),
            },
        ]);

        let details = metadata.field_details();
        assert!(details.contains(&("order_id".to_string(), "string".to_string(), true)));
        assert!(details.contains(&("total_amount".to_string(), "decimal".to_string(), true)));
        assert!(details.contains(&("placed_at".to_string(), "timestamp".to_string(), true)));
    }

    #[cfg(feature = "mssql_integration")]
    #[tokio::test]
    async fn feature_decodes_seeded_mssql_dates_and_prices() {
        let connection_string = std::env::var("MSSQL_CONNECTION_STRING").unwrap_or_else(|_| {
            "server=tcp:127.0.0.1,1433;database=testdb;user id=sa;password=Skippr!Test123;TrustServerCertificate=true".to_string()
        });
        let config = DataSourceMssqlPluginConfig {
            connection_string,
            tables: None,
            batch_size_rows: None,
            query_timeout_seconds: None,
            format: None,
            batch_size_bytes: None,
            batch_size_seconds: None,
        };
        let mut client = DataSourceMssqlPlugin::connect(&config).await.unwrap();
        let stream = client
            .simple_query(
                "SELECT o.order_id, o.total_amount, o.placed_at, i.unit_price \
                 FROM dbo.orders o \
                 JOIN dbo.order_items i ON i.order_id = o.order_id \
                 WHERE o.order_id = 'o1' AND i.order_item_id = 'oi1'",
            )
            .await
            .unwrap();
        let rows: Vec<Row> = stream.into_first_result().await.unwrap();
        let row = rows.first().expect("seeded MSSQL row should exist");
        let value: serde_json::Value =
            serde_json::from_str(&DataSourceMssqlPlugin::row_to_json(row)).unwrap();

        assert_eq!(value["order_id"], serde_json::json!("o1"));
        assert_eq!(value["total_amount"], serde_json::json!("120.50"));
        assert_eq!(value["unit_price"], serde_json::json!("50.00"));
        assert_eq!(value["placed_at"], serde_json::json!("2025-01-03T09:15:00"));
    }
}
