/// CDC End-to-End: Kafka source → Postgres sink
///
/// Requires Docker services: `kafka` and `postgres-target` (port 15433).
/// Tests 1–2 use Kafka on **19092** (see `docker-compose.yml`). Tests 3–4 use
/// `KAFKA_LIFECYCLE_PORT` (**9092**); expose or remap the broker if your host
/// does not listen on that port.
///
///   cargo test --test cdc_e2e_kafka -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const KAFKA_PORT: u16 = 19092;
const TARGET_PORT: u16 = 15433;

/// Kafka broker port for lifecycle / replay tests (`127.0.0.1:9092`). Adjust
/// host port mapping if your environment differs from `docker-compose.yml`.
const KAFKA_LIFECYCLE_PORT: u16 = 9092;
const PG_LIFECYCLE_PORT: u16 = 15433;

fn unique_topic(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

async fn produce_kafka_messages(topic: &str, messages: &[&str]) {
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{FutureProducer, FutureRecord};

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("127.0.0.1:{}", KAFKA_PORT))
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Kafka producer creation failed");

    for msg in messages {
        producer
            .send(
                FutureRecord::to(topic).payload(msg.as_bytes()).key("test"),
                Duration::from_secs(5),
            )
            .await
            .expect("Kafka send failed");
    }
}

async fn produce_debezium_messages(topic: &str, events: &[serde_json::Value]) {
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{FutureProducer, FutureRecord};

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("127.0.0.1:{}", KAFKA_PORT))
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Kafka producer creation failed");

    for event in events {
        let payload = serde_json::to_string(event).unwrap();
        producer
            .send(
                FutureRecord::to(topic)
                    .payload(payload.as_bytes())
                    .key("test"),
                Duration::from_secs(5),
            )
            .await
            .expect("Kafka send failed");
    }
}

// ---------------------------------------------------------------------------
// Test 1: Non-Debezium CDC — raw JSON records with insert semantics
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_kafka_cdc_raw_messages_reach_target() {
    let topic = unique_topic("cdc_kf");

    produce_kafka_messages(
        &topic,
        &[
            r#"{"id": 1, "name": "Alice", "city": "London"}"#,
            r#"{"id": 2, "name": "Bob", "city": "Paris"}"#,
        ],
    )
    .await;

    let target_table = format!("kafka.{}", topic);
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config = kafka_to_pg_cdc_config_yaml(
        KAFKA_PORT,
        TARGET_PORT,
        "cdc_kf_raw",
        &topic,
        &format!("skippr_e2e_{}", rand::random::<u32>()),
        &["id"],
        false,
    );

    let harness = CdcE2eHarness::new("cdc_kf_raw", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(count >= 2, "expected at least 2 rows, got {}", count);

    let has_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(has_token, "target should have _skippr_order_token");
}

// ---------------------------------------------------------------------------
// Test 2: Debezium envelope CDC
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_kafka_debezium_cdc_reaches_target() {
    let topic = unique_topic("cdc_kf_dbz");

    produce_debezium_messages(
        &topic,
        &[
            serde_json::json!({
                "payload": {
                    "op": "c",
                    "after": {"id": 10, "name": "Deb1", "city": "NYC"},
                    "source": {"ts_ms": 1700000000}
                }
            }),
            serde_json::json!({
                "payload": {
                    "op": "c",
                    "after": {"id": 20, "name": "Deb2", "city": "LA"},
                    "source": {"ts_ms": 1700000001}
                }
            }),
            serde_json::json!({
                "payload": {
                    "op": "u",
                    "after": {"id": 10, "name": "Deb1-Updated", "city": "NYC"},
                    "source": {"ts_ms": 1700000002}
                }
            }),
        ],
    )
    .await;

    let target_table = format!("kafka.{}", topic);
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config = kafka_to_pg_cdc_config_yaml(
        KAFKA_PORT,
        TARGET_PORT,
        "cdc_kf_dbz",
        &topic,
        &format!("skippr_dbz_{}", rand::random::<u32>()),
        &["id"],
        true,
    );

    let harness = CdcE2eHarness::new("cdc_kf_dbz", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(
        count >= 2,
        "expected at least 2 rows (insert + update on same key), got {}",
        count
    );

    let has_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(has_token, "target should have _skippr_order_token");
}

// ---------------------------------------------------------------------------
// Test 3: Debezium create → update → delete lifecycle (two sync passes)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_kafka_debezium_cdc_update_delete_lifecycle() {
    let topic = unique_topic("kafka_life");
    let broker = format!("127.0.0.1:{}", KAFKA_LIFECYCLE_PORT);

    produce_debezium_event(
        &broker,
        &topic,
        "c",
        "",
        r#"{"id":1,"name":"Alice","city":"London"}"#,
    );
    produce_debezium_event(
        &broker,
        &topic,
        "c",
        "",
        r#"{"id":2,"name":"Bob","city":"Paris"}"#,
    );
    produce_debezium_event(
        &broker,
        &topic,
        "c",
        "",
        r#"{"id":3,"name":"Carol","city":"Berlin"}"#,
    );

    let target_table = format!("kafka.{}", topic);
    let target = pg_client(PG_LIFECYCLE_PORT).await;
    pg_execute(
        &target,
        &format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ),
    )
    .await;

    let config = kafka_to_pg_cdc_config_yaml(
        KAFKA_LIFECYCLE_PORT,
        PG_LIFECYCLE_PORT,
        "kafka_life",
        &topic,
        "skippr-kafka-life",
        &["id"],
        true,
    );

    let harness = CdcE2eHarness::new("kafka_life", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr (lifecycle pass 1) ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    assert_eq!(pg_row_count(&target, &fq).await, 3);
    assert!(pg_row_exists(&target, &fq, "id = 1 AND name = 'Alice'").await);
    assert!(pg_row_exists(&target, &fq, "id = 2 AND name = 'Bob'").await);
    assert!(pg_row_exists(&target, &fq, "id = 3 AND name = 'Carol'").await);

    produce_debezium_event(
        &broker,
        &topic,
        "u",
        "",
        r#"{"id":2,"name":"Bob II","city":"Paris"}"#,
    );
    produce_debezium_event(
        &broker,
        &topic,
        "d",
        "",
        r#"{"id":3,"name":"Carol","city":"Berlin"}"#,
    );

    let stderr2 = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr (lifecycle pass 2) ---\n{}", stderr2);

    assert_eq!(pg_row_count(&target, &fq).await, 2);
    assert!(pg_row_exists(&target, &fq, "id = 1 AND name = 'Alice'").await);
    assert!(pg_row_exists(&target, &fq, "id = 2 AND name = 'Bob II'").await);
    assert!(!pg_row_exists(&target, &fq, "id = 3").await);
}

// ---------------------------------------------------------------------------
// Test 4: Running sync twice does not duplicate rows (committed offsets)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_kafka_cdc_replay_is_idempotent() {
    let topic = unique_topic("kafka_replay");
    let broker = format!("127.0.0.1:{}", KAFKA_LIFECYCLE_PORT);

    produce_debezium_event(
        &broker,
        &topic,
        "c",
        "",
        r#"{"id":1,"name":"A","city":"X"}"#,
    );
    produce_debezium_event(
        &broker,
        &topic,
        "c",
        "",
        r#"{"id":2,"name":"B","city":"Y"}"#,
    );

    let target_table = format!("kafka.{}", topic);
    let target = pg_client(PG_LIFECYCLE_PORT).await;
    pg_execute(
        &target,
        &format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ),
    )
    .await;

    let config = kafka_to_pg_cdc_config_yaml(
        KAFKA_LIFECYCLE_PORT,
        PG_LIFECYCLE_PORT,
        "kafka_replay",
        &topic,
        &format!("skippr-replay-{}", rand::random::<u32>()),
        &["id"],
        true,
    );

    let harness = CdcE2eHarness::new("kafka_replay", &config);
    harness.run_sync_with_timeout(Duration::from_secs(20));

    let fq = format!("\"public\".\"{}\"", target_table);
    assert_eq!(pg_row_count(&target, &fq).await, 2);

    harness.run_sync_with_timeout(Duration::from_secs(20));
    assert_eq!(
        pg_row_count(&target, &fq).await,
        2,
        "second sync should not duplicate CDC rows"
    );
}
