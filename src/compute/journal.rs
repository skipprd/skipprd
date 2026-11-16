//! Declared compute lives in `StoreFamily::Journal` under `compute#workspace#pipeline`.
//! The journal is a map keyed by compute name. StreamState holds cursor and measures only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skippr_wal_catalog::{CatalogFence, KvOp, SkipprStore, SkipprStoreRead, StoreFamily};

use crate::migration::journal::JournalKey;

use super::parse::canonicalize_sql_expr;
use super::{ComputeKind, Fqn, StatelessSelect};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComputeHash(pub String);

impl ComputeHash {
    pub fn of_kind(kind: &ComputeKind) -> Result<Self, String> {
        let canonical = canonicalize_kind(kind)?;
        let bytes = bincode::serialize(&canonical).map_err(|err| err.to_string())?;
        Ok(Self(format!("{:x}", md5::compute(bytes))))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub kind: ComputeKind,
    pub hash: ComputeHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ComputeLog {
    pub items: BTreeMap<String, CatalogEntry>,
}

impl ComputeLog {
    pub fn kinds(&self) -> Vec<ComputeKind> {
        self.items
            .values()
            .map(|entry| entry.kind.clone())
            .collect()
    }

    pub fn get(&self, name: &Fqn) -> Option<&CatalogEntry> {
        self.items.get(&name.to_string())
    }

    pub fn insert(&mut self, kind: ComputeKind) -> Result<(), String> {
        let hash = ComputeHash::of_kind(&kind)?;
        self.items
            .insert(kind.catalog_key(), CatalogEntry { kind, hash });
        Ok(())
    }
}

pub fn journal_key(workspace: &str, pipeline: &str) -> Result<JournalKey, String> {
    JournalKey::compute(workspace, pipeline)
}

fn store_key(key: &JournalKey) -> Vec<u8> {
    StoreFamily::Journal.key(key.as_str().as_bytes())
}

pub fn ir_hash_kind(kind: &ComputeKind) -> Result<ComputeHash, String> {
    ComputeHash::of_kind(kind)
}

pub fn ir_hash(kinds: &[ComputeKind]) -> Result<ComputeHash, String> {
    let mut log = ComputeLog::default();
    for kind in kinds {
        log.insert(kind.clone())?;
    }
    let bytes = bincode::serialize(&log).map_err(|err| err.to_string())?;
    Ok(ComputeHash(format!("{:x}", md5::compute(bytes))))
}

pub fn canonicalize_kind(kind: &ComputeKind) -> Result<ComputeKind, String> {
    let mut kind = kind.clone();
    match &mut kind {
        ComputeKind::Stateless { select, .. } | ComputeKind::View { select, .. } => {
            *select = canonicalize_select(select)?;
        }
        _ => {}
    }
    Ok(kind)
}

fn canonicalize_select(select: &StatelessSelect) -> Result<StatelessSelect, String> {
    Ok(StatelessSelect {
        filter: select
            .filter
            .as_ref()
            .map(|expr| canonicalize_sql_expr(expr).map_err(|err| err.to_string()))
            .transpose()?,
        derive: select
            .derive
            .iter()
            .map(|(name, expr)| {
                Ok((
                    name.clone(),
                    canonicalize_sql_expr(expr).map_err(|err| err.to_string())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?,
    })
}

pub fn load(store: &dyn SkipprStoreRead, key: &JournalKey) -> Result<ComputeLog, String> {
    match store.get(&store_key(key)).map_err(|err| err.to_string())? {
        None => Ok(ComputeLog::default()),
        Some(bytes) => bincode::deserialize(&bytes).map_err(|err| err.to_string()),
    }
}

pub fn encoded_log(log: &ComputeLog) -> Result<Vec<u8>, String> {
    bincode::serialize(log).map_err(|err| err.to_string())
}

pub fn put_op(key: &JournalKey, log: &ComputeLog) -> Result<KvOp, String> {
    Ok(KvOp::Put {
        key: store_key(key),
        value: encoded_log(log)?,
    })
}

pub fn commit(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    key: &JournalKey,
    kinds: Vec<ComputeKind>,
) -> Result<ComputeLog, String> {
    let mut log = ComputeLog::default();
    for kind in kinds {
        log.insert(kind)?;
    }
    store
        .commit(&[put_op(key, &log)?], fence)
        .map_err(|err| err.to_string())?;
    Ok(log)
}

pub fn commit_log(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    key: &JournalKey,
    log: &ComputeLog,
) -> Result<(), String> {
    store
        .commit(&[put_op(key, log)?], fence)
        .map_err(|err| err.to_string())
}

pub fn commit_with(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    ops: Vec<KvOp>,
) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    store.commit(&ops, fence).map_err(|err| err.to_string())
}

pub fn drop_compute(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    key: &JournalKey,
    name: &Fqn,
) -> Result<ComputeLog, String> {
    let mut log = load(store, key)?;
    log.items.remove(&name.to_string());
    commit_log(store, fence, key, &log)?;
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{
        FieldId, Fqn, Ident, Measure, MeasureKind, NonEmpty, PipelineName, Relation,
    };
    use skippr_wal_catalog::MemorySkipprStore;

    fn keyed() -> ComputeKind {
        ComputeKind::KeyedTable {
            name: Fqn::from_parts("invoices", "customer_totals").unwrap(),
            input: Relation::PipelineTable {
                pipeline: PipelineName::new("invoices").unwrap(),
                namespace: Ident::new("orders").unwrap(),
            },
            keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
            measures: NonEmpty::try_from_vec(vec![Measure {
                name: Ident::new("invoice_total").unwrap(),
                kind: MeasureKind::Sum(FieldId::new(2).unwrap()),
            }])
            .unwrap(),
        }
    }

    #[test]
    fn journal_key_is_compute_namespace() {
        let key = journal_key("ws", "invoices").unwrap();
        assert!(key.as_str().starts_with("compute#"));
        assert!(key.as_str().contains("invoices"));
        assert!(!key.as_str().contains("migration#"));
    }

    #[test]
    fn catalog_is_keyed_by_name_not_index() {
        let mut log = ComputeLog::default();
        log.insert(keyed()).unwrap();
        assert!(log.items.contains_key("invoices.customer_totals"));
        assert_eq!(
            log.get(&Fqn::from_parts("invoices", "customer_totals").unwrap())
                .unwrap()
                .kind
                .name()
                .to_string(),
            "invoices.customer_totals"
        );
    }

    #[test]
    fn whitespace_equivalent_sql_shares_hash() {
        use crate::compute::StatelessSelect;
        let a = ComputeKind::Stateless {
            name: Fqn::from_parts("invoices", "positive_orders").unwrap(),
            target: Relation::PipelineTable {
                pipeline: PipelineName::new("invoices").unwrap(),
                namespace: Ident::new("orders").unwrap(),
            },
            select: StatelessSelect {
                filter: Some("amount > 0".into()),
                derive: Vec::new(),
            },
        };
        let b = ComputeKind::Stateless {
            name: a.name().clone(),
            target: match &a {
                ComputeKind::Stateless { target, .. } => target.clone(),
                _ => unreachable!(),
            },
            select: StatelessSelect {
                filter: Some("amount>0".into()),
                derive: Vec::new(),
            },
        };
        assert_eq!(ir_hash_kind(&a).unwrap(), ir_hash_kind(&b).unwrap());
    }

    #[test]
    fn hash_stable_for_same_field_ids() {
        let a = ir_hash_kind(&keyed()).unwrap();
        let b = ir_hash_kind(&keyed()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn commit_and_load_round_trip() {
        let fence = CatalogFence::local();
        let store = MemorySkipprStore::open(fence);
        let key = journal_key("ws", "invoices").unwrap();
        commit(&store, fence, &key, vec![keyed()]).unwrap();
        let log = load(&store, &key).unwrap();
        assert_eq!(log.items.len(), 1);
        let entry = log
            .get(&Fqn::from_parts("invoices", "customer_totals").unwrap())
            .unwrap();
        assert_eq!(entry.hash, ir_hash_kind(&entry.kind).unwrap());
    }
}
