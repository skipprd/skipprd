#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Resolves the path to the `skippr-el` binary built by Cargo.
pub fn skippr_el_bin() -> PathBuf {
    std::env::var_os("SKIPPR_E2E_SKIPPR_EL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_skippr-el")))
}

/// Self-contained E2E harness that drives the real `skippr-el sync` binary
/// against Docker-compose services with an isolated DATA_DIR.
pub struct CdcE2eHarness {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub pipeline_name: String,
}

impl CdcE2eHarness {
    pub fn new(pipeline_name: &str, config_yaml: &str) -> Self {
        let id: u64 = rand::random();
        let data_dir = std::env::temp_dir().join(format!("skippr_cdc_e2e_{}", id));
        std::fs::create_dir_all(&data_dir).unwrap();

        let config_path = data_dir.join("skippr-el.yml");
        std::fs::write(&config_path, config_yaml).unwrap();

        CdcE2eHarness {
            config_path,
            data_dir,
            pipeline_name: pipeline_name.to_string(),
        }
    }

    /// Spawn `skippr-el sync` in the background and return the child handle.
    pub fn spawn_sync(&self) -> Child {
        let bin = skippr_el_bin();
        Command::new(&bin)
            .args(["sync", "--pipeline", &self.pipeline_name])
            .env("SKIPPR_CONFIG_FILE", &self.config_path)
            .env("DATA_DIR", &self.data_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {:?}: {}", bin, e))
    }

    /// Run the pipeline for up to `timeout`, then kill and return stderr output.
    pub fn run_sync_with_timeout(&self, timeout: Duration) -> String {
        let mut child = self.spawn_sync();
        std::thread::sleep(timeout);
        let _ = child.kill();
        let output = child.wait_with_output().expect("failed to collect output");
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl Drop for CdcE2eHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

// ---------------------------------------------------------------------------
// Config generation helpers
// ---------------------------------------------------------------------------

pub fn pg_cdc_config_yaml(
    source_port: u16,
    sink_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
) -> String {
    let tables_yaml: String = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {sink_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.pg_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn mysql_to_pg_cdc_config_yaml(
    mysql_port: u16,
    sink_port: u16,
    pipeline_name: &str,
    source_table: &str,
    business_keys: &[&str],
) -> String {
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  mysql_cdc_source:
    Mysql:
      connection_string: "mysql://skippr:testpass@127.0.0.1:{mysql_port}/skippr_test"
      tables: ["{source_table}"]

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {sink_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  {pipeline_name}:
    data_source: data_sources.mysql_cdc_source
    data_sink: data_sinks.pg_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn mongodb_to_pg_cdc_config_yaml(
    mongo_port: u16,
    sink_port: u16,
    pipeline_name: &str,
    database: &str,
    collection: &str,
    business_keys: &[&str],
) -> String {
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  mongo_cdc_source:
    Mongodb:
      connection_string: "mongodb://127.0.0.1:{mongo_port}/?directConnection=true"
      database: {database}
      collection: {collection}

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {sink_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  {pipeline_name}:
    data_source: data_sources.mongo_cdc_source
    data_sink: data_sinks.pg_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn dynamodb_to_pg_cdc_config_yaml(
    ddb_port: u16,
    sink_port: u16,
    pipeline_name: &str,
    table_name: &str,
    business_keys: &[&str],
) -> String {
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  ddb_cdc_source:
    Dynamodb:
      table_name: {table_name}
      region: us-east-1
      endpoint_url: "http://127.0.0.1:{ddb_port}"

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {sink_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  {pipeline_name}:
    data_source: data_sources.ddb_cdc_source
    data_sink: data_sinks.pg_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn kafka_to_pg_cdc_config_yaml(
    kafka_port: u16,
    sink_port: u16,
    pipeline_name: &str,
    topic: &str,
    group_id: &str,
    business_keys: &[&str],
    debezium: bool,
) -> String {
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    let debezium_str = if debezium { "true" } else { "false" };

    format!(
        r#"data_sources:
  kafka_cdc_source:
    Kafka:
      brokers: "127.0.0.1:{kafka_port}"
      topic: {topic}
      group_id: {group_id}
      mode: batch
      cdc: true
      debezium: {debezium_str}

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {sink_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  {pipeline_name}:
    data_source: data_sources.kafka_cdc_source
    data_sink: data_sinks.pg_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_clickhouse_cdc_config_yaml(
    source_port: u16,
    ch_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
) -> String {
    let tables_yaml: String = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  ch_cdc_target:
    Clickhouse:
      url: "http://127.0.0.1:{ch_port}"
      database: default

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.ch_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

// ---------------------------------------------------------------------------
// Postgres helper
// ---------------------------------------------------------------------------

pub async fn pg_client(port: u16) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(
        &format!(
            "host=127.0.0.1 port={} user=postgres password=testpass dbname=skippr_test",
            port
        ),
        tokio_postgres::NoTls,
    )
    .await
    .expect("pg connect failed");
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("pg connection error: {}", e);
        }
    });
    client
}

pub async fn pg_execute(client: &tokio_postgres::Client, sql: &str) {
    client
        .batch_execute(sql)
        .await
        .unwrap_or_else(|e| panic!("pg_execute failed: {}\nSQL: {}", e, sql));
}

pub async fn pg_row_count(client: &tokio_postgres::Client, table: &str) -> i64 {
    let row = client
        .query_one(&format!("SELECT COUNT(*)::bigint FROM {}", table), &[])
        .await
        .unwrap_or_else(|e| panic!("pg_row_count failed for {}: {}", table, e));
    row.get::<_, i64>(0)
}

pub async fn pg_column_exists(client: &tokio_postgres::Client, table: &str, column: &str) -> bool {
    let sql = format!(
        "SELECT 1 FROM information_schema.columns WHERE table_name = '{}' AND column_name = '{}'",
        table, column
    );
    let rows = client.query(&sql, &[]).await.unwrap();
    !rows.is_empty()
}

pub async fn pg_table_exists(client: &tokio_postgres::Client, table: &str) -> bool {
    let sql = format!(
        "SELECT 1 FROM information_schema.tables WHERE table_name = '{}'",
        table
    );
    let rows = client.query(&sql, &[]).await.unwrap();
    !rows.is_empty()
}

/// Wait until a table has at least `min_rows`, polling every 500ms for up to `timeout`.
pub async fn pg_wait_for_rows(
    client: &tokio_postgres::Client,
    table: &str,
    min_rows: i64,
    timeout: Duration,
) -> i64 {
    let start = std::time::Instant::now();
    loop {
        if let Ok(row) = client
            .query_one(&format!("SELECT COUNT(*)::bigint FROM {}", table), &[])
            .await
        {
            let count: i64 = row.get(0);
            if count >= min_rows {
                return count;
            }
        }
        if start.elapsed() >= timeout {
            panic!(
                "Timed out waiting for {} rows in {} after {:?}",
                min_rows, table, timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Fetch a single scalar value from Postgres.
pub async fn pg_query_value<T: for<'a> tokio_postgres::types::FromSql<'a>>(
    client: &tokio_postgres::Client,
    sql: &str,
) -> T {
    let row = client.query_one(sql, &[]).await.unwrap();
    row.get::<_, T>(0)
}

/// Fetch all `_skippr_order_token` values for a table.
pub async fn pg_order_tokens(client: &tokio_postgres::Client, table: &str) -> Vec<String> {
    let sql = format!("SELECT _skippr_order_token FROM {} ORDER BY 1", table);
    let rows = client.query(&sql, &[]).await.unwrap_or_default();
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

/// Check a specific row exists (by single-column predicate).
pub async fn pg_row_exists(
    client: &tokio_postgres::Client,
    table: &str,
    where_clause: &str,
) -> bool {
    let sql = format!("SELECT 1 FROM {} WHERE {}", table, where_clause);
    let rows = client.query(&sql, &[]).await.unwrap();
    !rows.is_empty()
}

// ---------------------------------------------------------------------------
// MySQL helpers
// ---------------------------------------------------------------------------

pub async fn mysql_pool(port: u16) -> mysql_async::Pool {
    let url = format!("mysql://skippr:testpass@127.0.0.1:{}/skippr_test", port);
    mysql_async::Pool::new(url.as_str())
}

pub async fn mysql_exec(pool: &mysql_async::Pool, sql: &str) {
    use mysql_async::prelude::Queryable;
    let mut conn = pool.get_conn().await.unwrap();
    conn.query_drop(sql).await.unwrap();
}

pub async fn mysql_exec_update(pool: &mysql_async::Pool, sql: &str) {
    mysql_exec(pool, sql).await;
}

pub async fn mysql_exec_delete(pool: &mysql_async::Pool, sql: &str) {
    mysql_exec(pool, sql).await;
}

// ---------------------------------------------------------------------------
// MongoDB helpers
// ---------------------------------------------------------------------------

pub async fn mongo_client(port: u16) -> mongodb::Client {
    let uri = format!("mongodb://127.0.0.1:{}/?directConnection=true", port);
    let opts = mongodb::options::ClientOptions::parse(&uri).await.unwrap();
    mongodb::Client::with_options(opts).unwrap()
}

pub async fn insert_mongo_doc(
    client: &mongodb::Client,
    database: &str,
    collection: &str,
    doc: mongodb::bson::Document,
) {
    client
        .database(database)
        .collection::<mongodb::bson::Document>(collection)
        .insert_one(doc)
        .await
        .unwrap();
}

pub async fn update_mongo_doc(
    client: &mongodb::Client,
    database: &str,
    collection: &str,
    filter: mongodb::bson::Document,
    update: mongodb::bson::Document,
) {
    client
        .database(database)
        .collection::<mongodb::bson::Document>(collection)
        .update_one(filter, update)
        .await
        .unwrap();
}

pub async fn delete_mongo_doc(
    client: &mongodb::Client,
    database: &str,
    collection: &str,
    filter: mongodb::bson::Document,
) {
    client
        .database(database)
        .collection::<mongodb::bson::Document>(collection)
        .delete_one(filter)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// DynamoDB helpers (LocalStack)
// ---------------------------------------------------------------------------

pub fn ddb_client(port: u16) -> aws_sdk_dynamodb::Client {
    let config = aws_sdk_dynamodb::Config::builder()
        .endpoint_url(format!("http://127.0.0.1:{}", port))
        .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_dynamodb::config::Credentials::new(
            "test", "test", None, None, "static",
        ))
        .behavior_version_latest()
        .build();
    aws_sdk_dynamodb::Client::from_conf(config)
}

pub async fn put_ddb_item(
    client: &aws_sdk_dynamodb::Client,
    table: &str,
    item: std::collections::HashMap<String, aws_sdk_dynamodb::types::AttributeValue>,
) {
    client
        .put_item()
        .table_name(table)
        .set_item(Some(item))
        .send()
        .await
        .unwrap();
}

pub async fn delete_ddb_item(
    client: &aws_sdk_dynamodb::Client,
    table: &str,
    key: std::collections::HashMap<String, aws_sdk_dynamodb::types::AttributeValue>,
) {
    client
        .delete_item()
        .table_name(table)
        .set_key(Some(key))
        .send()
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Kafka helpers
// ---------------------------------------------------------------------------

pub fn produce_kafka_message(broker: &str, topic: &str, payload: &str) {
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{BaseProducer, BaseRecord, Producer};

    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", broker)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("kafka producer creation failed");

    producer
        .send(BaseRecord::to(topic).payload(payload).key(""))
        .expect("kafka send failed");
    producer
        .flush(Duration::from_secs(5))
        .expect("kafka flush failed");
}

pub fn produce_debezium_event(
    broker: &str,
    topic: &str,
    op: &str,
    key_json: &str,
    value_json: &str,
) {
    let envelope = match op {
        "d" => format!(
            r#"{{"schema":null,"payload":{{"before":{},"after":null,"op":"d","source":{{}}}}}}"#,
            value_json,
        ),
        "u" => format!(
            r#"{{"schema":null,"payload":{{"before":null,"after":{},"op":"u","source":{{}}}}}}"#,
            value_json,
        ),
        _ => format!(
            r#"{{"schema":null,"payload":{{"before":null,"after":{},"op":"c","source":{{}}}}}}"#,
            value_json,
        ),
    };
    let _ = key_json; // key is part of the partition strategy, but for testing we use simple payloads
    produce_kafka_message(broker, topic, &envelope);
}

// ---------------------------------------------------------------------------
// Cloud sink config YAML generators
// ---------------------------------------------------------------------------

pub fn pg_to_snowflake_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    account: &str,
    user: &str,
    private_key_path: &str,
    warehouse: &str,
    database: &str,
    schema: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  sf_cdc_target:
    Snowflake:
      account: {account}
      user: {user}
      private_key_path: {private_key_path}
      warehouse: {warehouse}
      database: {database}
      schema: {schema}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.sf_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_bigquery_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    project: &str,
    dataset: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  bq_cdc_target:
    Bigquery:
      project: {project}
      dataset: {dataset}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.bq_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_databricks_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    host: &str,
    token: &str,
    catalog: &str,
    schema: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  db_cdc_target:
    Databricks:
      host: {host}
      token: {token}
      catalog: {catalog}
      schema: {schema}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.db_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_redshift_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    host: &str,
    user: &str,
    password: &str,
    database: &str,
    schema: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  rs_cdc_target:
    Redshift:
      host: {host}
      user: {user}
      password: {password}
      database: {database}
      schema: {schema}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.rs_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_synapse_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    connection_string: &str,
    schema: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  syn_cdc_target:
    Synapse:
      connection_string: "{connection_string}"
      schema: {schema}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.syn_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

pub fn pg_to_motherduck_cdc_config_yaml(
    source_port: u16,
    pipeline_name: &str,
    source_tables: &[&str],
    business_keys: &[&str],
    token: &str,
    database: &str,
) -> String {
    let tables_yaml = source_tables
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let bk_yaml = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: [{tables_yaml}]

data_sinks:
  md_cdc_target:
    Motherduck:
      token: {token}
      database: {database}

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.md_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]

"#
    )
}

// ---------------------------------------------------------------------------
// ClickHouse helpers
// ---------------------------------------------------------------------------

pub async fn ch_query(port: u16, sql: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(&format!("http://127.0.0.1:{}/", port))
        .body(sql.to_string())
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap()
}

pub async fn ch_exec(port: u16, sql: &str) {
    ch_query(port, sql).await;
}

pub async fn ch_row_count(port: u16, table: &str) -> i64 {
    let result = ch_query(port, &format!("SELECT count() FROM {}", table)).await;
    result.trim().parse::<i64>().unwrap_or(0)
}
