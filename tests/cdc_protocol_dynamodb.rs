/// CDC Protocol Tests: DynamoDB Streams  (secondary coverage)
///
/// Low-level tests that validate DynamoDB Streams protocol details.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_dynamodb` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker service `localstack`.
///
/// Run: `cargo test --test cdc_protocol_dynamodb -- --ignored`
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

// -----------------------------------------------------------------------
// 1. Stream discovery: table has a stream, list_streams returns it
// -----------------------------------------------------------------------
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
    assert!(
        !stream_list.is_empty(),
        "expected at least one stream for table"
    );

    let arn = stream_list[0].stream_arn().expect("stream has no ARN");
    assert!(
        arn.contains("stream"),
        "ARN should contain 'stream': {}",
        arn
    );
}

// -----------------------------------------------------------------------
// 2. Shard iteration: describe_stream returns shards
// -----------------------------------------------------------------------
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
        .expect("put_item failed");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let list_resp = streams
        .list_streams()
        .table_name(table)
        .send()
        .await
        .unwrap();
    let arn = list_resp.streams()[0].stream_arn().unwrap().to_string();

    let desc = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .expect("describe_stream failed");

    let stream_desc = desc.stream_description().expect("no description");
    let shards = stream_desc.shards();
    assert!(
        !shards.is_empty(),
        "expected at least one shard after a write"
    );

    let shard_id = shards[0].shard_id().expect("shard has no id");
    assert!(!shard_id.is_empty());
}

// -----------------------------------------------------------------------
// 3. Insert event: put_item produces an INSERT stream record
// -----------------------------------------------------------------------
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

    let desc = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap();
    let shard_id = desc.stream_description().unwrap().shards()[0]
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

    let shard_iter = iter.shard_iterator().unwrap();

    let records_resp = streams
        .get_records()
        .shard_iterator(shard_iter)
        .send()
        .await
        .unwrap();

    let records = records_resp.records();
    assert!(!records.is_empty(), "expected at least one INSERT record");

    let first = &records[0];
    assert_eq!(
        first.event_name(),
        Some(&aws_sdk_dynamodbstreams::types::OperationType::Insert)
    );

    let sr = first.dynamodb().expect("no stream record");
    let new_image = sr.new_image().expect("INSERT should have new_image");
    assert!(new_image.contains_key("pk"));

    let seq = sr
        .sequence_number()
        .expect("record should have sequence_number");
    assert!(!seq.is_empty());
}

// -----------------------------------------------------------------------
// 4. Update event: overwriting an item produces a MODIFY record
// -----------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn cdc_dynamodb_update_event() {
    let ddb = ddb_client().await;
    let streams = streams_client().await;
    let table = "cdc_ddb_update_test";

    ensure_table(&ddb, table).await;

    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S("row-u".into()))
        .item("data", AttributeValue::S("v1".into()))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S("row-u".into()))
        .item("data", AttributeValue::S("v2".into()))
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
        .unwrap();
    let shard_id = desc.stream_description().unwrap().shards()[0]
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

    let shard_iter = iter.shard_iterator().unwrap();

    let records_resp = streams
        .get_records()
        .shard_iterator(shard_iter)
        .send()
        .await
        .unwrap();

    let records = records_resp.records();
    assert!(
        records.len() >= 2,
        "expected at least 2 records (insert + modify), got {}",
        records.len()
    );

    let modify_records: Vec<_> = records
        .iter()
        .filter(|r| r.event_name() == Some(&aws_sdk_dynamodbstreams::types::OperationType::Modify))
        .collect();

    assert!(
        !modify_records.is_empty(),
        "expected at least one MODIFY record"
    );
}

// -----------------------------------------------------------------------
// 5. Delete event: delete_item produces a REMOVE record
// -----------------------------------------------------------------------
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

    let desc = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap();
    let shard_id = desc.stream_description().unwrap().shards()[0]
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

    let shard_iter = iter.shard_iterator().unwrap();

    let mut all_records = Vec::new();
    let mut current_iter = shard_iter.to_string();

    for _ in 0..5 {
        let resp = streams
            .get_records()
            .shard_iterator(&current_iter)
            .send()
            .await
            .unwrap();
        all_records.extend(resp.records().to_vec());
        match resp.next_shard_iterator() {
            Some(next) => current_iter = next.to_string(),
            None => break,
        }
        if !resp.records().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let remove_records: Vec<_> = all_records
        .iter()
        .filter(|r| r.event_name() == Some(&aws_sdk_dynamodbstreams::types::OperationType::Remove))
        .collect();

    assert!(
        !remove_records.is_empty(),
        "expected at least one REMOVE record, got {} total records",
        all_records.len()
    );

    let sr = remove_records[0]
        .dynamodb()
        .expect("no stream record on REMOVE");
    let keys = sr.keys().expect("REMOVE should have keys");
    assert!(keys.contains_key("pk"));
}

// -----------------------------------------------------------------------
// 6. Sequence number ordering: later writes have larger sequence numbers
// -----------------------------------------------------------------------
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
            .item("pk", AttributeValue::S(format!("ord-{}", i)))
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

    let desc = streams
        .describe_stream()
        .stream_arn(&arn)
        .send()
        .await
        .unwrap();
    let shard_id = desc.stream_description().unwrap().shards()[0]
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
    let mut all_records = Vec::new();

    for _ in 0..10 {
        let resp = streams
            .get_records()
            .shard_iterator(&current_iter)
            .send()
            .await
            .unwrap();
        all_records.extend(resp.records().to_vec());
        match resp.next_shard_iterator() {
            Some(next) => current_iter = next.to_string(),
            None => break,
        }
        if all_records.len() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    assert!(
        all_records.len() >= 2,
        "expected at least 2 records for ordering check"
    );

    let seq_numbers: Vec<&str> = all_records
        .iter()
        .filter_map(|r| r.dynamodb().and_then(|sr| sr.sequence_number()))
        .collect();

    for pair in seq_numbers.windows(2) {
        assert!(
            pair[0] <= pair[1],
            "sequence numbers must be non-decreasing: {} > {}",
            pair[0],
            pair[1]
        );
    }
}
