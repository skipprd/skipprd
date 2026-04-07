/// CDC End-to-End: MongoDB source → Postgres sink
///
/// Requires Docker services: `mongodb` (port 17017 for snapshot test; 27017 for lifecycle /
/// replay tests) and `postgres-target` (port 15433).
///
///   cargo test --test cdc_e2e_mongodb -- --ignored
mod support;

use std::time::Duration;

use mongodb::bson::doc;
use support::cdc_e2e::*;

const MONGO_PORT: u16 = 17017;
const MONGO_PORT_LIFE: u16 = 27017;
const TARGET_PORT: u16 = 15433;

fn unique_collection(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

async fn seed_mongo(collection: &str, docs: &[serde_json::Value]) {
    let client = mongo_client(MONGO_PORT).await;
    let db = client.database("skippr_test");
    let _ = db
        .collection::<mongodb::bson::Document>(collection)
        .drop()
        .await;
    let coll = db.collection::<mongodb::bson::Document>(collection);
    for doc in docs {
        let bson_doc = mongodb::bson::to_document(doc).unwrap();
        coll.insert_one(bson_doc).await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn e2e_mongodb_cdc_snapshot_reaches_target() {
    let coll = unique_collection("cdc_mg");

    seed_mongo(
        &coll,
        &[
            serde_json::json!({"_id": 1, "name": "Alice", "city": "London"}),
            serde_json::json!({"_id": 2, "name": "Bob", "city": "Paris"}),
            serde_json::json!({"_id": 3, "name": "Charlie", "city": "Berlin"}),
        ],
    )
    .await;

    let target_table = format!("mongodb.{}", coll);
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config = mongodb_to_pg_cdc_config_yaml(
        MONGO_PORT,
        TARGET_PORT,
        "cdc_mg_snap",
        "skippr_test",
        &coll,
        &["_id"],
    );

    let harness = CdcE2eHarness::new("cdc_mg_snap", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(count >= 3, "expected at least 3 rows, got {}", count);

    let has_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(has_token, "target should have _skippr_order_token");
}

#[tokio::test]
#[ignore]
async fn e2e_mongodb_cdc_insert_update_delete_lifecycle() {
    let collection_name = unique_collection("mongo_life");
    let client = mongo_client(MONGO_PORT_LIFE).await;
    let db = client.database("skippr_test");
    let _ = db
        .collection::<mongodb::bson::Document>(&collection_name)
        .drop()
        .await;

    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "1", "name": "Alice", "city": "London" },
    )
    .await;
    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "2", "name": "Bob", "city": "Paris" },
    )
    .await;
    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "3", "name": "Charlie", "city": "Berlin" },
    )
    .await;

    let target_table = format!("mongodb.skippr_test.{}", collection_name);
    let target = pg_client(TARGET_PORT).await;
    pg_execute(
        &target,
        &format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ),
    )
    .await;

    let config = mongodb_to_pg_cdc_config_yaml(
        MONGO_PORT_LIFE,
        TARGET_PORT,
        "mongo_life",
        "skippr_test",
        &collection_name,
        &["_id"],
    );

    {
        let harness = CdcE2eHarness::new("mongo_life", &config);
        let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
        eprintln!("--- skippr-el stderr (lifecycle pass 1) ---\n{}", stderr);
    }

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(
        count >= 3,
        "expected at least 3 rows after first sync, got {}",
        count
    );

    update_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "2" },
        doc! { "$set": { "name": "Bob_Updated" } },
    )
    .await;
    delete_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "3" },
    )
    .await;

    {
        let harness = CdcE2eHarness::new("mongo_life", &config);
        let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
        eprintln!("--- skippr-el stderr (lifecycle pass 2) ---\n{}", stderr);
    }

    let count_final = pg_row_count(&target, &fq).await;
    assert_eq!(
        count_final, 2,
        "expected 2 rows after update+delete sync, got {}",
        count_final
    );
    assert!(
        pg_row_exists(&target, &fq, "\"_id\" = '1' AND \"name\" = 'Alice'").await,
        "Alice row should remain"
    );
    assert!(
        pg_row_exists(&target, &fq, "\"_id\" = '2' AND \"name\" = 'Bob_Updated'").await,
        "Bob should be updated"
    );
    assert!(
        !pg_row_exists(&target, &fq, "\"_id\" = '3'").await,
        "Charlie row should be deleted"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_mongodb_cdc_replay_is_idempotent() {
    let collection_name = unique_collection("mongo_replay");
    let client = mongo_client(MONGO_PORT_LIFE).await;
    let db = client.database("skippr_test");
    let _ = db
        .collection::<mongodb::bson::Document>(&collection_name)
        .drop()
        .await;

    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "1", "name": "Alice", "city": "London" },
    )
    .await;
    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "2", "name": "Bob", "city": "Paris" },
    )
    .await;
    insert_mongo_doc(
        &client,
        "skippr_test",
        &collection_name,
        doc! { "_id": "3", "name": "Charlie", "city": "Berlin" },
    )
    .await;

    let target_table = format!("mongodb.skippr_test.{}", collection_name);
    let target = pg_client(TARGET_PORT).await;
    pg_execute(
        &target,
        &format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ),
    )
    .await;

    let config = mongodb_to_pg_cdc_config_yaml(
        MONGO_PORT_LIFE,
        TARGET_PORT,
        "mongo_replay",
        "skippr_test",
        &collection_name,
        &["_id"],
    );

    {
        let harness = CdcE2eHarness::new("mongo_replay", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    let fq = format!("\"public\".\"{}\"", target_table);
    let count_after_first = pg_row_count(&target, &fq).await;
    assert!(
        count_after_first >= 3,
        "expected at least 3 rows after first sync, got {}",
        count_after_first
    );

    {
        let harness = CdcE2eHarness::new("mongo_replay", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    let count_after_second = pg_row_count(&target, &fq).await;
    assert_eq!(
        count_after_first, count_after_second,
        "replay should be idempotent: first={} second={}",
        count_after_first, count_after_second
    );
}
