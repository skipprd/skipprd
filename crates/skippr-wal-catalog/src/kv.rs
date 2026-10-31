//! One Skippr Store trait. Families share this KV. Live impls: memory and fjall.

use std::sync::Arc;

use crate::error::CatalogError;
use crate::row::CatalogFence;

pub const STORE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFamily {
    StoreMeta,
    Catalog,
    Offsets,
    StreamState,
    Schema,
    Journal,
}

impl StoreFamily {
    pub fn prefix(self) -> &'static [u8] {
        match self {
            Self::StoreMeta => b"store_meta/",
            Self::Catalog => b"catalog/",
            Self::Offsets => b"offsets/",
            Self::StreamState => b"stream_state/",
            Self::Schema => b"schema/",
            Self::Journal => b"journal/",
        }
    }

    pub fn key(self, rest: &[u8]) -> Vec<u8> {
        let mut out = self.prefix().to_vec();
        out.extend_from_slice(rest);
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KvOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

pub trait SkipprStoreRead: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError>;
    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError>;
}

pub trait SkipprStore: SkipprStoreRead {
    fn commit(&self, ops: &[KvOp], fence: CatalogFence) -> Result<(), CatalogError>;
    fn flush_durable(&self) -> Result<(), CatalogError>;
    fn pin(&self) -> Result<Arc<dyn SkipprStoreRead>, CatalogError>;
}

pub fn family_rest<'a>(family: StoreFamily, key: &'a [u8]) -> Option<&'a [u8]> {
    key.strip_prefix(family.prefix())
}

impl<T: SkipprStoreRead + ?Sized> SkipprStoreRead for Arc<T> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError> {
        (**self).get(key)
    }

    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
        (**self).query_prefix(prefix, after, limit)
    }
}

impl<T: SkipprStore + ?Sized> SkipprStore for Arc<T> {
    fn commit(&self, ops: &[KvOp], fence: CatalogFence) -> Result<(), CatalogError> {
        (**self).commit(ops, fence)
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        (**self).flush_durable()
    }

    fn pin(&self) -> Result<Arc<dyn SkipprStoreRead>, CatalogError> {
        (**self).pin()
    }
}
