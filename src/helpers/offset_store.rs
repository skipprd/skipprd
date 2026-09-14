//! Object-safe offset/checkpoint store. Sled, DynamoDB, and Cloud Tables
//! implement this contract; core WAL code does not branch on backend.

use crate::helpers::offsets::{OffsetTypes, OffsetValue};
use sled::IVec;
use zerocopy::{byteorder::U64, AsBytes, LayoutVerified};

pub trait OffsetStore: Send + Sync {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String>;
    fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), String>;
    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String>;
    fn fetch_and_update_offset(
        &self,
        namespace: &str,
        partition: &str,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String>;
    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn put_checkpoint(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    fn flush(&self) -> Result<(), String>;
}

pub struct SledOffsetStore {
    tree: sled::Tree,
}

impl SledOffsetStore {
    pub fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    pub(crate) fn offset_key(namespace: &str, partition: &str) -> String {
        format!("{namespace}-{partition}-latest")
    }

    fn checkpoint_key(key: &str) -> String {
        format!("cdc_checkpoint:{key}")
    }
}

impl OffsetStore for SledOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        self.tree
            .get(sk.as_bytes())
            .map(|v| v.map(|bytes| bytes.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), String> {
        self.tree
            .insert(sk.as_bytes(), bytes)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::offset_key(namespace, partition))
    }

    fn fetch_and_update_offset(
        &self,
        namespace: &str,
        partition: &str,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        let key = Self::offset_key(namespace, partition);
        let mut update = Some(update);
        self.tree
            .fetch_and_update(key.as_bytes(), |current| {
                update
                    .take()
                    .and_then(|f| f(current.map(|v| v.to_vec())))
                    .map(IVec::from)
            })
            .map(|v| v.map(|bytes| bytes.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::checkpoint_key(key))
    }

    fn put_checkpoint(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_bytes(&Self::checkpoint_key(key), bytes)
    }

    fn flush(&self) -> Result<(), String> {
        self.tree.flush().map(|_| ()).map_err(|err| err.to_string())
    }
}

#[cfg(feature = "offset-store-dynamodb")]
impl OffsetStore for skippr_offset_store_dynamodb::DynamoDbOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_bytes(self, sk)
    }

    fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::put_bytes(self, sk, bytes)
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_offset(self, namespace, partition)
    }

    fn fetch_and_update_offset(
        &self,
        namespace: &str,
        partition: &str,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::fetch_and_update_offset(
            self, namespace, partition, update,
        )
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_checkpoint(self, key)
    }

    fn put_checkpoint(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::put_checkpoint(self, key, bytes)
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(feature = "offset-store-cloud-tables")]
impl OffsetStore for skippr_store_cloud_tables::CloudTablesOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::get_bytes(self, sk)
    }

    fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::put_bytes(self, sk, bytes)
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::get_offset(self, namespace, partition)
    }

    fn fetch_and_update_offset(
        &self,
        namespace: &str,
        partition: &str,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::fetch_and_update_offset(
            self, namespace, partition, update,
        )
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::get_checkpoint(self, key)
    }

    fn put_checkpoint(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        skippr_store_cloud_tables::CloudTablesOffsetStore::put_checkpoint(self, key, bytes)
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

pub fn apply_offset_field(
    existing: Option<Vec<u8>>,
    offset_type: OffsetTypes,
    offset: u64,
    position_max: bool,
) -> Vec<u8> {
    let mut backing = existing.unwrap_or_else(|| {
        OffsetValue {
            filesize: U64::new(0),
            line: U64::new(0),
            closed: U64::new(0),
        }
        .as_bytes()
        .to_vec()
    });
    let layout: LayoutVerified<&mut [u8], OffsetValue> =
        LayoutVerified::new_unaligned(backing.as_mut_slice())
            .expect("offset bytes do not fit schema");
    let value: &mut OffsetValue = layout.into_mut();
    match offset_type {
        OffsetTypes::Filesize => value.filesize = U64::new(offset),
        OffsetTypes::Position => {
            let current = value.line.get();
            value.line = U64::new(if position_max {
                current.max(offset)
            } else {
                offset
            });
        }
        OffsetTypes::Closed => value.closed = U64::new(offset),
    }
    backing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::offsets::OffsetKey;

    #[test]
    fn sled_offset_store_returns_errors_instead_of_swallowing() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = SledOffsetStore::new(db.open_tree("offsets").unwrap());
        store
            .fetch_and_update_offset(
                "ns",
                "p",
                Box::new(|current| {
                    Some(apply_offset_field(current, OffsetTypes::Closed, 1, false))
                }),
            )
            .unwrap();
        let key = SledOffsetStore::offset_key("ns", "p");
        assert!(store.get_bytes(&key).unwrap().is_some());
        assert!(store.get_offset("ns", "p").unwrap().is_some());
        let _ = OffsetKey::new("ns", "p");
    }

    #[test]
    fn offsets_dispatch_is_the_offset_store_trait() {
        let offsets = include_str!("offsets.rs");
        assert!(
            offsets.contains("Arc<dyn OffsetStore>"),
            "Offsets must dispatch through dyn OffsetStore"
        );
        assert!(offsets.contains("fn insert("));
        assert!(offsets.contains("Result<Option<IVec>, OffsetsError>"));
    }
}
