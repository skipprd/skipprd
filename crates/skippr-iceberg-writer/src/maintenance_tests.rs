//! Automatic maintenance end to end against a filesystem catalog.

use super::grouped_tests::{scanned_ids, Harness, NAMESPACE};
use super::*;
use arrow::array::Int64Array;
use skippr_runtime_sdk::plugins::cdc::{
    MutationKind, NamespaceContract, SyncContext, WalPartMeta, WalRowMeta,
};

/// Seeds the idle totals so the writer's own cheap gate skips every pass,
/// giving tests table states that maintenance has not touched yet.
async fn hold_maintenance(harness: &Harness) {
    harness.writer.maintenance_idle.lock().await.insert(
        NAMESPACE.to_string(),
        maintenance::TableTotals {
            data_files: u64::MAX / 2,
            delete_files: u64::MAX / 2,
            bytes: 0,
        },
    );
}

async fn release_maintenance(harness: &Harness) {
    harness.writer.maintenance_idle.lock().await.clear();
}

async fn held_appends(harness: &Harness, count: i64) {
    hold_maintenance(harness).await;
    for i in 0..count {
        harness.write(&i.to_string(), i..i + 1).await;
    }
    release_maintenance(harness).await;
}

async fn merge_write(harness: &Harness, key: &str, ids: &[i64]) {
    let contract = SourceNamespaceContract {
        namespace: NAMESPACE.to_string(),
        primary_key: vec![FieldPath::single("id")],
        cursor: None,
        partition_key: Vec::new(),
        write_policy: WritePolicy::MergeByKey,
        refresh_window: None,
        description: String::new(),
        semantics: None,
    };
    harness
        .write_rows(
            &harness.writer,
            key,
            ids.to_vec(),
            None,
            Some(&contract),
            None,
        )
        .await;
}

async fn cdc_write(harness: &Harness, key: &str, events: &[(i64, MutationKind, u8)]) {
    let rows = events
        .iter()
        .map(|(id, mutation, token)| WalRowMeta {
            mutation: *mutation,
            event_id: vec![*id as u8, *token],
            order_token: vec![0, *token],
        })
        .collect::<Vec<_>>();
    let cdc = SyncContext {
        part_meta: WalPartMeta::cdc(rows, events.len() as u64).unwrap(),
        contract: Some(NamespaceContract {
            namespace: NAMESPACE.to_string(),
            business_key_columns: vec!["id".to_string()],
            effective_guarantee: EffectiveGuarantee::ExactOnceFinalState,
            order_token_semantics: Default::default(),
            null_key_policy: Default::default(),
            requires_skippr_system_columns: true,
        }),
    };
    harness
        .write_rows(
            &harness.writer,
            key,
            events.iter().map(|(id, _, _)| *id).collect(),
            None,
            None,
            Some(&cdc),
        )
        .await;
}

#[derive(Debug, Default)]
struct LiveFiles {
    data: Vec<String>,
    deletes: Vec<String>,
}

async fn live_files(table: &iceberg::table::Table) -> LiveFiles {
    let mut live = LiveFiles::default();
    let Some(snapshot) = table.metadata().current_snapshot() else {
        return live;
    };
    let list = snapshot
        .load_manifest_list(table.file_io(), table.metadata())
        .await
        .unwrap();
    for manifest in list.entries() {
        for entry in manifest
            .load_manifest(table.file_io())
            .await
            .unwrap()
            .entries()
        {
            if !entry.is_alive() {
                continue;
            }
            match entry.content_type() {
                DataContentType::Data => live.data.push(entry.file_path().to_string()),
                _ => live.deletes.push(entry.file_path().to_string()),
            }
        }
    }
    live
}

fn maintenance_snapshots(table: &iceberg::table::Table) -> Vec<iceberg::spec::Operation> {
    table
        .metadata()
        .snapshots()
        .filter(|snapshot| {
            snapshot
                .summary()
                .additional_properties
                .contains_key(maintenance::SNAPSHOT_MAINTENANCE)
        })
        .map(|snapshot| snapshot.summary().operation.clone())
        .collect()
}

fn total(table: &iceberg::table::Table, key: &str) -> u64 {
    table
        .metadata()
        .current_snapshot()
        .unwrap()
        .summary()
        .additional_properties
        .get(key)
        .unwrap()
        .parse()
        .unwrap()
}

fn data_dir_files(harness: &Harness) -> usize {
    std::fs::read_dir(
        harness
            .dir
            .path()
            .join("lake")
            .join(harness.writer.table_name(NAMESPACE))
            .join("data"),
    )
    .unwrap()
    .count()
}

async fn rows_with_tokens(table: &iceberg::table::Table) -> Vec<(i64, String)> {
    let batches: Vec<RecordBatch> = table
        .scan()
        .select_all()
        .build()
        .unwrap()
        .to_arrow()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let mut rows = Vec::new();
    for batch in &batches {
        let ids = batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let tokens = batch
            .column_by_name("_skippr_order_token")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            rows.push((ids.value(row), tokens.value(row).to_string()));
        }
    }
    rows.sort();
    rows
}

#[tokio::test]
async fn small_grouped_files_collapse_and_keep_every_row() {
    let harness = Harness::new().await;
    for i in 0..40_i64 {
        harness.write(&i.to_string(), i..i + 1).await;
    }

    let table = harness.table().await;
    assert_eq!(
        maintenance_snapshots(&table),
        vec![iceberg::spec::Operation::Replace]
    );
    let live = live_files(&table).await;
    assert_eq!(live.data.len(), 40 - 33 + 1);
    assert_eq!(total(&table, "total-data-files"), live.data.len() as u64);
    assert_eq!(total(&table, "total-records"), 40);
    assert_eq!(scanned_ids(&table).await, (0..40).collect::<Vec<_>>());
}

#[tokio::test]
async fn merge_deletes_are_applied_and_removed_with_the_whole_partition() {
    let harness = Harness::new().await;
    hold_maintenance(&harness).await;
    for i in 0..=maintenance::PARTITION_DELETE_FILES_TRIGGER {
        merge_write(&harness, &format!("merge-{i}"), &[0, 1, 2, 3, 4]).await;
    }
    release_maintenance(&harness).await;
    let before = harness.table().await;
    assert_eq!(
        live_files(&before).await.deletes.len(),
        maintenance::PARTITION_DELETE_FILES_TRIGGER + 1
    );
    assert_eq!(scanned_ids(&before).await, vec![0, 1, 2, 3, 4]);

    harness.write("trigger", 100..101).await;

    let table = harness.table().await;
    assert_eq!(
        maintenance_snapshots(&table),
        vec![iceberg::spec::Operation::Overwrite]
    );
    let live = live_files(&table).await;
    assert!(live.deletes.is_empty(), "{live:?}");
    assert_eq!(live.data.len(), 1);
    assert_eq!(total(&table, "total-delete-files"), 0);
    assert_eq!(total(&table, "total-data-files"), 1);
    assert_eq!(scanned_ids(&table).await, vec![0, 1, 2, 3, 4, 100]);
}

#[tokio::test]
async fn cdc_final_state_and_order_tokens_are_unchanged_by_a_rewrite() {
    let harness = Harness::new().await;
    hold_maintenance(&harness).await;
    for round in 1..=20_u8 {
        let deleted = if round <= 18 {
            (2, MutationKind::Delete, round)
        } else {
            (2, MutationKind::Insert, round)
        };
        cdc_write(
            &harness,
            &format!("cdc-{round}"),
            &[
                (0, MutationKind::Update, round),
                (1, MutationKind::Update, round),
                deleted,
            ],
        )
        .await;
    }
    release_maintenance(&harness).await;
    let before = harness.table().await;
    let expected = rows_with_tokens(&before).await;
    assert_eq!(expected.iter().filter(|(id, _)| *id == 2).count(), 2);
    assert!(live_files(&before).await.deletes.len() > maintenance::PARTITION_DELETE_FILES_TRIGGER);

    let after = harness
        .writer
        .maintenance_pass(NAMESPACE, &before)
        .await
        .unwrap()
        .expect("deletes past the trigger plan a rewrite");

    assert_eq!(rows_with_tokens(&after).await, expected);
    assert!(live_files(&after).await.deletes.is_empty());
    assert_eq!(rows_with_tokens(&harness.table().await).await, expected);
}

#[tokio::test]
async fn a_conflicting_commit_aborts_the_pass_and_the_next_pass_succeeds() {
    let harness = Harness::new().await;
    held_appends(
        &harness,
        maintenance::PARTITION_DATA_FILES_TRIGGER as i64 + 1,
    )
    .await;
    let stale = harness.table().await;
    hold_maintenance(&harness).await;
    harness.write("late", 1_000..1_001).await;
    release_maintenance(&harness).await;
    let current = harness.table().await;

    assert!(harness
        .writer
        .maintenance_pass(NAMESPACE, &stale)
        .await
        .is_err());
    let table = harness.table().await;
    assert_eq!(
        table.metadata().current_snapshot_id(),
        current.metadata().current_snapshot_id()
    );
    assert!(maintenance_snapshots(&table).is_empty());

    let compacted = harness
        .writer
        .maintenance_pass(NAMESPACE, &current)
        .await
        .unwrap()
        .unwrap();
    let mut expected: Vec<i64> = (0..=maintenance::PARTITION_DATA_FILES_TRIGGER as i64).collect();
    expected.push(1_000);
    assert_eq!(scanned_ids(&compacted).await, expected);
    assert_eq!(live_files(&compacted).await.data.len(), 1);
}

#[tokio::test]
async fn a_failed_pass_backs_off_until_more_files_arrive() {
    let harness = Harness::new().await;
    held_appends(
        &harness,
        maintenance::PARTITION_DATA_FILES_TRIGGER as i64 + 1,
    )
    .await;
    let stale = harness.table().await;
    hold_maintenance(&harness).await;
    harness.write("late", 1_000..1_001).await;
    release_maintenance(&harness).await;

    harness.writer.maintain(NAMESPACE, stale).await;

    let current = harness.table().await;
    assert!(harness
        .writer
        .maintenance_pass(NAMESPACE, &current)
        .await
        .unwrap()
        .is_none());
    assert!(maintenance_snapshots(&current).is_empty());
}

#[tokio::test]
async fn crash_after_upload_replays_from_the_receipt_without_duplicates() {
    let harness = Harness::new().await;
    held_appends(
        &harness,
        maintenance::PARTITION_DATA_FILES_TRIGGER as i64 + 1,
    )
    .await;
    let table = harness.table().await;
    let plan = harness.writer.plan_maintenance(&table).await.unwrap();
    harness
        .writer
        .prepare_maintenance(NAMESPACE, &table, &plan)
        .await
        .unwrap();
    let uploaded = data_dir_files(&harness);

    let committed = harness
        .writer
        .maintenance_pass(NAMESPACE, &table)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(data_dir_files(&harness), uploaded);
    assert_eq!(
        scanned_ids(&committed).await,
        (0..=maintenance::PARTITION_DATA_FILES_TRIGGER as i64).collect::<Vec<_>>()
    );
    assert_eq!(live_files(&committed).await.data.len(), 1);
    assert!(harness
        .writer
        .maintenance_pass(NAMESPACE, &committed)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn one_pass_rewrites_at_most_the_file_ceiling() {
    let harness = Harness::new().await;
    let files = maintenance::PASS_MAX_INPUT_FILES + 6;
    held_appends(&harness, files as i64).await;

    let committed = harness
        .writer
        .maintenance_pass(NAMESPACE, &harness.table().await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        live_files(&committed).await.data.len(),
        files - maintenance::PASS_MAX_INPUT_FILES + 1
    );
    assert_eq!(
        scanned_ids(&committed).await,
        (0..files as i64).collect::<Vec<_>>()
    );
}
