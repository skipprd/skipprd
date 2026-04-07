/// CDC Protocol Tests: MongoDB  (secondary coverage)
///
/// Low-level tests that validate MongoDB Change Streams protocol details.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_mongodb` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker service `mongodb` (replica set).
///
/// Run: `cargo test --test cdc_protocol_mongodb -- --ignored`
use futures::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::change_stream::event::OperationType;
use mongodb::options::{ClientOptions, FullDocumentType};
use mongodb::Client;

const MONGO_URI: &str = "mongodb://127.0.0.1:17017/?directConnection=true";

async fn connect() -> Option<Client> {
    let opts = match ClientOptions::parse(MONGO_URI).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("MongoDB connection string parse error (is the replica set up?): {e}");
            return None;
        }
    };
    match Client::with_options(opts) {
        Ok(c) => {
            // Ping to verify the replica set is reachable and initialized.
            if let Err(e) = c.database("admin").run_command(doc! { "ping": 1 }).await {
                eprintln!("MongoDB ping failed (replica set not ready?): {e}");
                return None;
            }
            Some(c)
        }
        Err(e) => {
            eprintln!("MongoDB client creation failed: {e}");
            None
        }
    }
}

fn test_collection(client: &Client, name: &str) -> mongodb::Collection<Document> {
    client.database("skippr_test").collection(name)
}

// -----------------------------------------------------------------------
// 1. Change stream receives insert
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mongodb_change_stream_receives_insert() {
    let client = match connect().await {
        Some(c) => c,
        None => {
            eprintln!("skipping: MongoDB replica set not available");
            return;
        }
    };
    let coll = test_collection(&client, "cdc_insert_test");
    coll.drop().await.ok();

    let mut stream = coll.watch().await.expect("open change stream");

    coll.insert_one(doc! { "_id": 1, "name": "Alice" })
        .await
        .expect("insert");

    let event = stream
        .try_next()
        .await
        .expect("read change stream")
        .expect("expected at least one event");

    assert_eq!(event.operation_type, OperationType::Insert);
    let full = event.full_document.expect("fullDocument should be present");
    assert_eq!(full.get_str("name").unwrap(), "Alice");
    assert_eq!(full.get_i32("_id").unwrap(), 1);

    coll.drop().await.ok();
}

// -----------------------------------------------------------------------
// 2. Change stream captures update
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mongodb_change_stream_captures_update() {
    let client = match connect().await {
        Some(c) => c,
        None => {
            eprintln!("skipping: MongoDB replica set not available");
            return;
        }
    };
    let coll = test_collection(&client, "cdc_update_test");
    coll.drop().await.ok();

    coll.insert_one(doc! { "_id": 1, "name": "Alice", "age": 30 })
        .await
        .expect("seed insert");

    let mut stream = coll
        .watch()
        .full_document(FullDocumentType::UpdateLookup)
        .await
        .expect("open change stream with UpdateLookup");

    coll.update_one(doc! { "_id": 1 }, doc! { "$set": { "age": 31 } })
        .await
        .expect("update");

    let event = stream
        .try_next()
        .await
        .expect("read change stream")
        .expect("expected update event");

    assert_eq!(event.operation_type, OperationType::Update);
    let full = event.full_document.expect("fullDocument from UpdateLookup");
    assert_eq!(full.get_i32("age").unwrap(), 31);
    assert_eq!(full.get_str("name").unwrap(), "Alice");

    coll.drop().await.ok();
}

// -----------------------------------------------------------------------
// 3. Change stream captures delete
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mongodb_change_stream_captures_delete() {
    let client = match connect().await {
        Some(c) => c,
        None => {
            eprintln!("skipping: MongoDB replica set not available");
            return;
        }
    };
    let coll = test_collection(&client, "cdc_delete_test");
    coll.drop().await.ok();

    coll.insert_one(doc! { "_id": 42, "data": "bye" })
        .await
        .expect("seed insert");

    let mut stream = coll.watch().await.expect("open change stream");

    coll.delete_one(doc! { "_id": 42 }).await.expect("delete");

    let event = stream
        .try_next()
        .await
        .expect("read change stream")
        .expect("expected delete event");

    assert_eq!(event.operation_type, OperationType::Delete);
    let key = event.document_key.expect("documentKey should be present");
    assert_eq!(key.get_i32("_id").unwrap(), 42);

    coll.drop().await.ok();
}

// -----------------------------------------------------------------------
// 4. Resume token round-trip
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mongodb_resume_token_round_trip() {
    let client = match connect().await {
        Some(c) => c,
        None => {
            eprintln!("skipping: MongoDB replica set not available");
            return;
        }
    };
    let coll = test_collection(&client, "cdc_resume_test");
    coll.drop().await.ok();

    // Open stream and capture initial resume token.
    let mut stream = coll.watch().await.expect("open change stream");
    let _initial_token = stream
        .resume_token()
        .expect("initial resume token should exist");

    // Insert first document.
    coll.insert_one(doc! { "_id": 1, "val": "first" })
        .await
        .expect("insert first");

    let first_event = stream
        .try_next()
        .await
        .expect("read first event")
        .expect("expected first event");
    assert_eq!(first_event.operation_type, OperationType::Insert);
    let after_first_token = stream
        .resume_token()
        .expect("resume token after first event");

    // Drop the stream.
    drop(stream);

    // Reopen with resume_after — should only see events after the first insert.
    let mut stream2 = coll
        .watch()
        .resume_after(after_first_token)
        .await
        .expect("reopen change stream with resume token");

    coll.insert_one(doc! { "_id": 2, "val": "second" })
        .await
        .expect("insert second");

    let second_event = stream2
        .try_next()
        .await
        .expect("read second event")
        .expect("expected second event");

    assert_eq!(second_event.operation_type, OperationType::Insert);
    let full = second_event
        .full_document
        .expect("fullDocument for second event");
    assert_eq!(full.get_str("val").unwrap(), "second");
    assert_eq!(full.get_i32("_id").unwrap(), 2);

    coll.drop().await.ok();
}

// -----------------------------------------------------------------------
// 5. Anchor timestamp capture
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mongodb_anchor_timestamp_capture() {
    let client = match connect().await {
        Some(c) => c,
        None => {
            eprintln!("skipping: MongoDB replica set not available");
            return;
        }
    };
    let db = client.database("skippr_test");
    let coll = test_collection(&client, "cdc_anchor_test");
    coll.drop().await.ok();

    // Capture operationTime from serverStatus.
    let status1 = db
        .run_command(doc! { "serverStatus": 1 })
        .await
        .expect("serverStatus");
    let ts1 = status1
        .get_timestamp("operationTime")
        .expect("operationTime should be a BSON Timestamp");

    let encoded1 = (ts1.time as u64) << 32 | (ts1.increment as u64);
    let bytes1 = encoded1.to_be_bytes();
    assert_eq!(bytes1.len(), 8, "encoded timestamp should be 8 bytes");
    assert!(encoded1 > 0, "timestamp should be non-zero");

    // Insert a document so the operationTime advances.
    coll.insert_one(doc! { "_id": 1, "check": true })
        .await
        .expect("insert");

    let status2 = db
        .run_command(doc! { "serverStatus": 1 })
        .await
        .expect("serverStatus after insert");
    let ts2 = status2
        .get_timestamp("operationTime")
        .expect("operationTime after insert");

    let encoded2 = (ts2.time as u64) << 32 | (ts2.increment as u64);
    assert!(
        encoded2 >= encoded1,
        "operationTime should advance: {encoded1} -> {encoded2}"
    );

    coll.drop().await.ok();
}
