//! DST: leftover sled migrate crash-resume and ingest during copy.

use skippr_wal_catalog::{
    migrate_sled_db, CatalogFence, FjallSkipprStore, KvOp, MigrateStatus, SkipprStore,
    SkipprStoreRead, StoreFamily,
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
        match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
            MigrateStatus::InProgress { .. } => {}
            other => panic!("expected in-progress, got {other:?}"),
        }
        drop(store);
    }
    let store = FjallSkipprStore::open(&store_path, f).unwrap();
    loop {
        match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
            MigrateStatus::Complete { .. } => break,
            MigrateStatus::InProgress { .. } => {}
            MigrateStatus::NoSled => panic!("sled should exist until complete"),
        }
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
    match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
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
        skippr_wal_catalog::lookup_miss_in_sled(&sled_path, StoreFamily::Offsets, b"k079")
            .unwrap()
            .as_deref(),
        Some(b"v79".as_slice())
    );
}
