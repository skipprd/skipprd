/// CDC Protocol Tests: DynamoDB Streams (secondary coverage)
///
/// Low-level tests that validate DynamoDB Streams protocol details.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_dynamodb` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker service `localstack`.
///
/// Run: `cargo test -p skippr-plugin-runtime-link --features runtime-sink-link --test cdc_protocol_dynamodb -- --ignored`
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, KeySchemaElement, KeyType, ProvisionedThroughput,
    ScalarAttributeType, StreamSpecification, StreamViewType,
};
use aws_sdk_dynamodbstreams::types::ShardIteratorType;

const ENDPOINT: &str = "http://127.0.0.1:14566";
const REGION: &str = "us-east-1";

async fn ddb_client() -> aws_sdk_dynamodb::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_types::region::Region::new(REGION))
        .load()
        .await;
    let mut builder = aws_sdk_dynamodb::config::Builder::from(&config);
    builder = builder.endpoint_url(ENDPOINT);
    aws_sdk_dynamodb::Client::from_conf(builder.build())
}

async fn streams_client() -> aws_sdk_dynamodbstreams::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_types::region::Region::new(REGION))
        .load()
        .await;
    let mut builder = aws_sdk_dynamodbstreams::config::Builder::from(&config);
    builder = builder.endpoint_url(ENDPOINT);
    aws_sdk_dynamodbstreams::Client::from_conf(builder.build())
}

async fn ensure_table(ddb: &aws_sdk_dynamodb::Client, table: &str) {
    let _ = ddb.delete_table().table_name(table).send().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    ddb.create_table()
        .table_name(table)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(ScalarAttributeType::S)
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
        .expect("create_table failed");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
#[ignore]
async fn cdc_dynamodb_stream_discovery() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_discovery_test";

    ensure_table(&ddb, table).await;

    let resp = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .expect("list_streams failed");

    let stream_list = resp.streams();
    assert!(!stream_list.is_empty());
    assert!(stream_list[0].stream_arn().unwrap().contains("stream"));
}

#[tokio::test]
#[ignore]
async fn cdc_dynamodb_describe_stream_shards() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_shards_test";

    ensure_table(&ddb, table).await;
    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S("trigger".into()))
        .item("val", AttributeValue::N("1".into()))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let arn = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .streams()[0]
        .stream_arn()
        .unwrap()
        .to_string();

    let desc = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .expect("describe_stream failed");
    let shards = desc.stream_description().unwrap().shards();
    assert!(!shards.is_empty());
}

#[tokio::test]
#[ignore]
async fn cdc_dynamodb_insert_event() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_insert_test";

    ensure_table(&ddb, table).await;
    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S("row-1".into()))
        .item("data", AttributeValue::S("hello".into()))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let arn = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .streams()[0]
        .stream_arn()
        .unwrap()
        .to_string();
    let shard_id = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap()
        .stream_description()
        .unwrap()
        .shards()[0]
        .shard_id()
        .unwrap()
        .to_string();
    let iter = streams
        .get_shard_iterator()
        .stream_arn(&arn)
        .shard_id(&shard_id)
        .shard_iterator_type(ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();

    let records = streams
        .get_records()
        .shard_iterator(iter.shard_iterator().unwrap())
        .send()
        .await
        .unwrap()
        .records()
        .to_vec();
    assert!(!records.is_empty());
    assert_eq!(
        records[0].event_name(),
        Some(&aws_sdk_dynamodbstreams::types::OperationType::Insert)
    );
}

#[tokio::test]
#[ignore]
async fn cdc_dynamodb_delete_event() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_delete_test";

    ensure_table(&ddb, table).await;
    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S("row-d".into()))
        .item("data", AttributeValue::S("gone".into()))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ddb.delete_item()
        .table_name(table)
        .key("pk", AttributeValue::S("row-d".into()))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let arn = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .streams()[0]
        .stream_arn()
        .unwrap()
        .to_string();
    let shard_id = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap()
        .stream_description()
        .unwrap()
        .shards()[0]
        .shard_id()
        .unwrap()
        .to_string();
    let iter = streams
        .get_shard_iterator()
        .stream_arn(&arn)
        .shard_id(&shard_id)
        .shard_iterator_type(ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();

    let mut all_records: Vec<aws_sdk_dynamodbstreams::types::Record> = Vec::new();
    let mut current_iter = iter.shard_iterator().unwrap().to_string();
    for _ in 0..5 {
        let resp = streams
            .get_records()
            .shard_iterator(&current_iter)
            .send()
            .await
            .unwrap();
        all_records.extend(resp.records().to_vec());
        if let Some(next) = resp.next_shard_iterator() {
            current_iter = next.to_string();
        } else {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    assert!(all_records.iter().any(|record| {
        record.event_name() == Some(&aws_sdk_dynamodbstreams::types::OperationType::Remove)
    }));
}

#[tokio::test]
#[ignore]
async fn cdc_dynamodb_sequence_number_ordering() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_ordering_test";

    ensure_table(&ddb, table).await;
    for i in 0..5 {
        ddb.put_item()
            .table_name(table)
            .item("pk", AttributeValue::S(format!("ord-{i}")))
            .item("seq", AttributeValue::N(i.to_string()))
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let arn = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .streams()[0]
        .stream_arn()
        .unwrap()
        .to_string();
    let shard_id = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap()
        .stream_description()
        .unwrap()
        .shards()[0]
        .shard_id()
        .unwrap()
        .to_string();
    let iter = streams
        .get_shard_iterator()
        .stream_arn(&arn)
        .shard_id(&shard_id)
        .shard_iterator_type(ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();

    let mut current_iter = iter.shard_iterator().unwrap().to_string();
    let mut all_records: Vec<aws_sdk_dynamodbstreams::types::Record> = Vec::new();
    for _ in 0..10 {
        let resp = streams
            .get_records()
            .shard_iterator(&current_iter)
            .send()
            .await
            .unwrap();
        all_records.extend(resp.records().to_vec());
        if let Some(next) = resp.next_shard_iterator() {
            current_iter = next.to_string();
        } else {
            break;
        }
        if all_records.len() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let seq_numbers: Vec<&str> = all_records
        .iter()
        .filter_map(|record| record.dynamodb().and_then(|sr| sr.sequence_number()))
        .collect();
    for pair in seq_numbers.windows(2) {
        assert!(pair[0] <= pair[1]);
    }
}
