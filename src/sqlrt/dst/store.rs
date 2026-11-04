//! DST: leftover sled migrate.
//!
//! Unhappy paths covered here:
//! - crash mid-copy, reopen fjall, resume from cursor (no full rescan)
//! - lost cursor: start from first key, idempotent puts, live ingest keys stay
//! - corrupt cursor: fail closed
//! - ingest / lookup-miss while a batch is in flight
//! - second writer fail-closed during an in-progress copy
//! - leftover `db.sled-migrated-*` ignored
//! - missing sled path is NoSled
//! - missing tree is skipped; other trees still copy

use skippr_wal_catalog::{
    lookup_miss_in_sled, migrate_sled_db, migrate_sled_db_step, CatalogFence, FjallSkipprStore,
    KvOp, MigrateStatus, SkipprStore, SkipprStoreRead, StoreFamily,
};

fn seed_sled(path: &std::path::Path, n: u32) {
    let db = sled::open(path).unwrap();
    let tree = db.open_tree("offsets").unwrap();
    for i in 0..n {
        tree.insert(
            format!("k{i:03}").into_bytes(),
            format!("v{i}").into_bytes(),
        )
        .unwrap();
    }
    db.flush().unwrap();
}

#[test]
fn i_sled_migrate_crash_resume_from_cursor() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    seed_sled(&sled_path, 80);
    let f = CatalogFence::local();
    let store_path = root.path().join("skippr-store");
    {
        let store = FjallSkipprStore::open(&store_path, f).unwrap();
        match migrate_sled_db_step(&store, f, &sled_path, 1).unwrap() {
            MigrateStatus::InProgress { .. } => {}
            other => panic!("expected in-progress, got {other:?}"),
        }
        drop(store);
    }
    let store = FjallSkipprStore::open(&store_path, f).unwrap();
    match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::Complete { .. } => {}
        other => panic!("resume drain should complete, got {other:?}"),
    }
    assert_eq!(
        SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"k079"))
            .unwrap()
            .as_deref(),
        Some(b"v79".as_slice())
    );
}

#[test]
fn i_sled_migrate_ingest_during_copy() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    seed_sled(&sled_path, 80);
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    match migrate_sled_db_step(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::InProgress { .. } => {}
        other => panic!("expected in-progress, got {other:?}"),
    }
    SkipprStore::commit(
        &store,
        &[KvOp::Put {
            key: StoreFamily::Offsets.key(b"live"),
            value: b"ingest".to_vec(),
        }],
        f,
    )
    .unwrap();
    assert_eq!(
        SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"live"))
            .unwrap()
            .as_deref(),
        Some(b"ingest".as_slice())
    );
    assert_eq!(
        lookup_miss_in_sled(&sled_path, StoreFamily::Offsets, b"k079")
            .unwrap()
            .as_deref(),
        Some(b"v79".as_slice())
    );
}

#[test]
fn i_sled_migrate_lost_cursor_starts_over() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    seed_sled(&sled_path, 80);
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    match migrate_sled_db_step(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::InProgress { .. } => {}
        other => panic!("expected in-progress, got {other:?}"),
    }
    SkipprStore::commit(
        &store,
        &[
            KvOp::Put {
                key: StoreFamily::Offsets.key(b"live"),
                value: b"ingest".to_vec(),
            },
            KvOp::Delete {
                key: StoreFamily::StoreMeta.key(b"sled_migrate/cursor"),
            },
        ],
        f,
    )
    .unwrap();
    match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::Complete { .. } => {}
        other => panic!("expected complete, got {other:?}"),
    }
    assert_eq!(
        SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"k079"))
            .unwrap()
            .as_deref(),
        Some(b"v79".as_slice())
    );
    assert_eq!(
        SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"live"))
            .unwrap()
            .as_deref(),
        Some(b"ingest".as_slice())
    );
}

#[test]
fn i_sled_migrate_corrupt_cursor_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    seed_sled(&sled_path, 8);
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    SkipprStore::commit(
        &store,
        &[KvOp::Put {
            key: StoreFamily::StoreMeta.key(b"sled_migrate/cursor"),
            value: b"not-a-cursor".to_vec(),
        }],
        f,
    )
    .unwrap();
    assert!(
        migrate_sled_db(&store, f, &sled_path, 1).is_err(),
        "corrupt cursor must fail closed"
    );
}

#[test]
fn i_sled_migrate_second_writer_fail_closed_during_copy() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    seed_sled(&sled_path, 80);
    let f = CatalogFence::local();
    let store_path = root.path().join("skippr-store");
    let store = FjallSkipprStore::open(&store_path, f).unwrap();
    match migrate_sled_db_step(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::InProgress { .. } => {}
        other => panic!("expected in-progress, got {other:?}"),
    }
    let err = FjallSkipprStore::open(&store_path, f)
        .err()
        .expect("second writer must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("already writing") || msg.contains("lock"),
        "{msg}"
    );
}

#[test]
fn i_sled_migrate_leftover_archive_ignored() {
    let root = tempfile::tempdir().unwrap();
    let archived = root.path().join("db.sled-migrated-3");
    std::fs::create_dir_all(&archived).unwrap();
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    assert_eq!(
        migrate_sled_db(&store, f, &archived, 3).unwrap(),
        MigrateStatus::NoSled
    );
}

#[test]
fn i_sled_migrate_missing_path_is_nosled() {
    let root = tempfile::tempdir().unwrap();
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    assert_eq!(
        migrate_sled_db(&store, f, &root.path().join("db"), 1).unwrap(),
        MigrateStatus::NoSled
    );
}

#[test]
fn i_sled_migrate_skips_missing_tree() {
    let root = tempfile::tempdir().unwrap();
    let sled_path = root.path().join("db");
    let db = sled::open(&sled_path).unwrap();
    db.open_tree("stream_state")
        .unwrap()
        .insert(b"meta/p/id", b"st")
        .unwrap();
    db.flush().unwrap();
    drop(db);
    let f = CatalogFence::local();
    let store = FjallSkipprStore::open(root.path().join("skippr-store"), f).unwrap();
    match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
        MigrateStatus::Complete { .. } => {}
        other => panic!("expected complete, got {other:?}"),
    }
    assert_eq!(
        SkipprStoreRead::get(&store, &StoreFamily::StreamState.key(b"meta/p/id"))
            .unwrap()
            .as_deref(),
        Some(b"st".as_slice())
    );
}
