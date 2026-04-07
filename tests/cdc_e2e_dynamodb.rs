/// CDC End-to-End: DynamoDB source → Postgres sink
///
/// Requires Docker services: LocalStack (DynamoDB; default host port `4566`, or
/// `14566:4566` per `docker-compose.yml`) and `postgres-target` (port 15433).
///
///   cargo test --test cdc_e2e_dynamodb -- --ignored
mod support;

use std::collections::HashMap;
use std::time::Duration;

use aws_sdk_dynamodb::types::AttributeValue;
use support::cdc_e2e::*;

/// Host port for LocalStack when bound as `4566:4566` (see lifecycle / order-token tests).
const DDB_PORT: u16 = 4566;
/// Host port used by `docker-compose` mapping `14566:4566` (snapshot test).
const DDB_PORT_COMPOSE: u16 = 14566;
const TARGET_PORT: u16 = 15433;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

/// Local helper for tests that use the `14566:4566` compose mapping (shadows `cdc_e2e::ddb_client`).
async fn ddb_client_compose() -> aws_sdk_dynamodb::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(format!("http://127.0.0.1:{}", DDB_PORT_COMPOSE))
        .credentials_provider(aws_sdk_dynamodb::config::Credentials::new(
            "test", "test", None, None, "static",
        ))
        .load()
        .await;
    aws_sdk_dynamodb::Client::new(&config)
}

async fn ensure_ddb_table_id(client: &aws_sdk_dynamodb::Client, table: &str) {
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, KeySchemaElement, KeyType, ProvisionedThroughput, ScalarAttributeType,
        StreamSpecification, StreamViewType,
    };
    let _ = client.delete_table().table_name(table).send().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    client
        .create_table()
        .table_name(table)
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("id")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .provisioned_throughput(
            ProvisionedThroughput::builder()
                .read_capacity_units(5)
                .write_capacity_units(5)
                .build()
                .unwrap(),
        )
        .stream_specification(
            StreamSpecification::builder()
                .stream_enabled(true)
                .stream_view_type(StreamViewType::NewAndOldImages)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn ensure_ddb_table_pk(client: &aws_sdk_dynamodb::Client, table: &str) {
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, KeySchemaElement, KeyType, ProvisionedThroughput, ScalarAttributeType,
        StreamSpecification, StreamViewType,
    };
    let _ = client.delete_table().table_name(table).send().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    client
        .create_table()
        .table_name(table)
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .provisioned_throughput(
            ProvisionedThroughput::builder()
                .read_capacity_units(5)
                .write_capacity_units(5)
                .build()
                .unwrap(),
        )
        .stream_specification(
            StreamSpecification::builder()
                .stream_enabled(true)
                .stream_view_type(StreamViewType::NewAndOldImages)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn seed_ddb(client: &aws_sdk_dynamodb::Client, table: &str, items: &[(&str, &str, &str)]) {
    for (id, name, city) in items {
        client
            .put_item()
            .table_name(table)
            .item("id", AttributeValue::S(id.to_string()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("city", AttributeValue::S(city.to_string()))
            .send()
            .await
            .unwrap();
    }
}

fn ddb_item_pk(pk: &str, name: &str, city: &str) -> HashMap<String, AttributeValue> {
    let mut item = HashMap::new();
    item.insert("pk".to_string(), AttributeValue::S(pk.to_string()));
    item.insert("name".to_string(), AttributeValue::S(name.to_string()));
    item.insert("city".to_string(), AttributeValue::S(city.to_string()));
    item
}

async fn cleanup_dynamodb_pg_target(client: &tokio_postgres::Client, source_table: &str) {
    let target_table = format!("dynamodb.{}", source_table);
    let tombstone = format!("_skippr_tombstones_{}", target_table);
    let _ = client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;
    let _ = client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            tombstone
        ))
        .await;
}

#[tokio::test]
#[ignore]
async fn e2e_dynamodb_cdc_snapshot_reaches_target() {
    let table = unique_table("cdc_ddb");
    let client = ddb_client_compose().await;

    ensure_ddb_table_id(&client, &table).await;
    seed_ddb(
        &client,
        &table,
        &[("1", "Alice", "London"), ("2", "Bob", "Paris")],
    )
    .await;

    let target_table = format!("dynamodb.{}", table);
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config = dynamodb_to_pg_cdc_config_yaml(
        DDB_PORT_COMPOSE,
        TARGET_PORT,
        "cdc_ddb_snap",
        &table,
        &["id"],
    );

    let harness = CdcE2eHarness::new("cdc_ddb_snap", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(count >= 2, "expected at least 2 rows, got {}", count);
}

#[tokio::test]
#[ignore]
async fn e2e_dynamodb_cdc_insert_update_delete_lifecycle() {
    let table = unique_table("cdc_life_test");
    let client = support::cdc_e2e::ddb_client(DDB_PORT);

    ensure_ddb_table_pk(&client, &table).await;

    put_ddb_item(&client, &table, ddb_item_pk("1", "Alice", "London")).await;
    put_ddb_item(&client, &table, ddb_item_pk("2", "Bob", "Paris")).await;
    put_ddb_item(&client, &table, ddb_item_pk("3", "Charlie", "Berlin")).await;

    let target = pg_client(TARGET_PORT).await;
    cleanup_dynamodb_pg_target(&target, &table).await;

    let config = dynamodb_to_pg_cdc_config_yaml(DDB_PORT, TARGET_PORT, "ddb_life", &table, &["pk"]);

    let harness = CdcE2eHarness::new("ddb_life", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr (lifecycle sync 1) ---\n{}", stderr);

    let target_table = format!("dynamodb.{}", table);
    let fq = format!("\"public\".\"{}\"", target_table);

    let count_first = pg_row_count(&target, &fq).await;
    assert_eq!(
        count_first, 3,
        "expected 3 rows after first sync, got {}",
        count_first
    );
    assert!(
        pg_row_exists(
            &target,
            &fq,
            "pk = '1' AND name = 'Alice' AND city = 'London'"
        )
        .await,
        "row pk=1 should match initial snapshot",
    );
    assert!(
        pg_row_exists(&target, &fq, "pk = '2' AND name = 'Bob' AND city = 'Paris'").await,
        "row pk=2 should match initial snapshot",
    );
    assert!(
        pg_row_exists(
            &target,
            &fq,
            "pk = '3' AND name = 'Charlie' AND city = 'Berlin'"
        )
        .await,
        "row pk=3 should match initial snapshot",
    );

    put_ddb_item(&client, &table, ddb_item_pk("1", "Alicia", "Manchester")).await;

    let mut del_key = HashMap::new();
    del_key.insert("pk".to_string(), AttributeValue::S("2".to_string()));
    delete_ddb_item(&client, &table, del_key).await;

    let harness2 = CdcE2eHarness::new("ddb_life", &config);
    let stderr2 = harness2.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr (lifecycle sync 2) ---\n{}", stderr2);

    pg_wait_for_rows(&target, &fq, 2, Duration::from_secs(30)).await;

    assert!(
        pg_row_exists(
            &target,
            &fq,
            "pk = '1' AND name = 'Alicia' AND city = 'Manchester'"
        )
        .await,
        "pk=1 should reflect UPDATE",
    );
    assert!(
        !pg_row_exists(&target, &fq, "pk = '2'").await,
        "pk=2 should be deleted",
    );
    assert!(
        pg_row_exists(
            &target,
            &fq,
            "pk = '3' AND name = 'Charlie' AND city = 'Berlin'"
        )
        .await,
        "pk=3 should be unchanged",
    );
}

#[tokio::test]
#[ignore]
async fn e2e_dynamodb_cdc_order_token_populated() {
    let table = unique_table("cdc_life_test");
    let client = support::cdc_e2e::ddb_client(DDB_PORT);

    ensure_ddb_table_pk(&client, &table).await;
    put_ddb_item(&client, &table, ddb_item_pk("1", "Alice", "London")).await;
    put_ddb_item(&client, &table, ddb_item_pk("2", "Bob", "Paris")).await;

    let target = pg_client(TARGET_PORT).await;
    cleanup_dynamodb_pg_target(&target, &table).await;

    let config =
        dynamodb_to_pg_cdc_config_yaml(DDB_PORT, TARGET_PORT, "ddb_order_tok", &table, &["pk"]);

    let harness = CdcE2eHarness::new("ddb_order_tok", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let target_table = format!("dynamodb.{}", table);
    let fq = format!("\"public\".\"{}\"", target_table);

    let count = pg_row_count(&target, &fq).await;
    assert!(
        count >= 2,
        "expected at least 2 rows in target, got {}",
        count
    );

    let has_order_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(
        has_order_token,
        "target table should have _skippr_order_token column",
    );

    let non_null_tokens: i64 = target
        .query_one(
            &format!(
                "SELECT COUNT(*)::bigint FROM {} WHERE \"_skippr_order_token\" IS NOT NULL",
                fq
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        non_null_tokens >= 2,
        "snapshot rows should have non-null order tokens, got {}",
        non_null_tokens
    );
}
