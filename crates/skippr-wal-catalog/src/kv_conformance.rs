use crate::error::CatalogError;
use crate::kv::{KvOp, SkipprStore, SkipprStoreRead, StoreFamily};
use crate::row::CatalogFence;

pub fn assert_skippr_store_conformance(store: &dyn SkipprStore, fence: CatalogFence) {
    conditional_commit(store, fence);
    prefix_scan(store, fence);
    reader_pin(store, fence);
    no_leases_family(store);
}

fn conditional_commit(store: &dyn SkipprStore, fence: CatalogFence) {
    let key = StoreFamily::Offsets.key(b"ns-part");
    store
        .commit(
            &[KvOp::Put {
                key: key.clone(),
                value: b"one".to_vec(),
            }],
            fence,
        )
        .unwrap();
    assert_eq!(store.get(&key).unwrap().as_deref(), Some(b"one".as_slice()));
    store
        .commit(
            &[KvOp::Put {
                key: key.clone(),
                value: b"two".to_vec(),
            }],
            fence,
        )
        .unwrap();
    assert_eq!(store.get(&key).unwrap().as_deref(), Some(b"two".as_slice()));
    store
        .commit(&[KvOp::Delete { key: key.clone() }], fence)
        .unwrap();
    assert!(store.get(&key).unwrap().is_none());
}

fn prefix_scan(store: &dyn SkipprStore, fence: CatalogFence) {
    store
        .commit(
            &[
                KvOp::Put {
                    key: StoreFamily::Schema.key(b"p/head"),
                    value: b"h".to_vec(),
                },
                KvOp::Put {
                    key: StoreFamily::Schema.key(b"p/ns/a/node/1"),
                    value: b"n".to_vec(),
                },
            ],
            fence,
        )
        .unwrap();
    let page = store
        .query_prefix(StoreFamily::Schema.prefix(), None, 10)
        .unwrap();
    assert!(page.len() >= 2);
    assert!(page
        .iter()
        .all(|(k, _)| k.starts_with(StoreFamily::Schema.prefix())));
}

fn reader_pin(store: &dyn SkipprStore, fence: CatalogFence) {
    let key = StoreFamily::StoreMeta.key(b"pin-demo");
    store
        .commit(
            &[KvOp::Put {
                key: key.clone(),
                value: b"before".to_vec(),
            }],
            fence,
        )
        .unwrap();
    let pin = store.pin().unwrap();
    store
        .commit(
            &[KvOp::Put {
                key: key.clone(),
                value: b"after".to_vec(),
            }],
            fence,
        )
        .unwrap();
    assert_eq!(
        pin.get(&key).unwrap().as_deref(),
        Some(b"before".as_slice())
    );
    assert_eq!(
        store.get(&key).unwrap().as_deref(),
        Some(b"after".as_slice())
    );
}

fn no_leases_family(store: &dyn SkipprStoreRead) {
    let page = store.query_prefix(b"leases/", None, 8).unwrap();
    assert!(
        page.is_empty(),
        "Skippr Store must not grow a leases family, got {page:?}"
    );
}

pub fn assert_stale_fence_rejected(
    store: &dyn SkipprStore,
    good: CatalogFence,
    stale: CatalogFence,
) {
    let err = store
        .commit(
            &[KvOp::Put {
                key: StoreFamily::Offsets.key(b"fenced"),
                value: b"x".to_vec(),
            }],
            stale,
        )
        .unwrap_err();
    assert_eq!(err, CatalogError::Fenced);
    store
        .commit(
            &[KvOp::Put {
                key: StoreFamily::Offsets.key(b"fenced"),
                value: b"ok".to_vec(),
            }],
            good,
        )
        .unwrap();
}
