use dashmap::DashMap;
use once_cell::sync::Lazy;

use crate::buffer::compaction_transaction::CompactionTransaction;
use crate::plugins::source_contract::WritePolicy;

static ACTIVE_SINK_CONFLICT_KEYS: Lazy<DashMap<String, ()>> = Lazy::new(DashMap::new);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkConflictKey {
    pub key: String,
    pub parallel_safe: bool,
}

pub fn sink_conflict_key(txn: &CompactionTransaction) -> SinkConflictKey {
    let parallel_safe = matches!(txn.write_policy, WritePolicy::Append);
    let key = match txn.write_policy {
        WritePolicy::ReplaceTable => format!("replace_table:{}:{}", txn.sink_ref, txn.namespace),
        WritePolicy::ReplacePartition => {
            format!("replace_partition:{}:{}", txn.sink_ref, txn.target_filename)
        }
        WritePolicy::MergeByKey | WritePolicy::Append => {
            if parallel_safe {
                format!("append:{}:{}:{}", txn.sink_ref, txn.namespace, txn.id)
            } else {
                format!("serialize:{}:{}", txn.sink_ref, txn.namespace)
            }
        }
    };
    SinkConflictKey { key, parallel_safe }
}

pub fn try_acquire_sink_conflict(txn: &CompactionTransaction) -> Option<SinkConflictGuard> {
    let conflict = sink_conflict_key(txn);
    if conflict.parallel_safe {
        return Some(SinkConflictGuard { key: None });
    }
    if ACTIVE_SINK_CONFLICT_KEYS
        .insert(conflict.key.clone(), ())
        .is_some()
    {
        return None;
    }
    Some(SinkConflictGuard {
        key: Some(conflict.key),
    })
}

pub struct SinkConflictGuard {
    key: Option<String>,
}

impl Drop for SinkConflictGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            ACTIVE_SINK_CONFLICT_KEYS.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::compaction_transaction::{CompactionTransaction, SinkWriteSemantics};

    #[test]
    fn append_compactions_get_distinct_parallel_keys() {
        let mut txn_a = CompactionTransaction::new(
            "sink.main".to_string(),
            "ns".to_string(),
            "schema".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::IdempotentAtLeastOnce,
            Vec::new(),
            "a".to_string(),
        );
        txn_a.id = "compaction-a".to_string();
        let mut txn_b = txn_a.clone();
        txn_b.id = "compaction-b".to_string();
        let key_a = sink_conflict_key(&txn_a);
        let key_b = sink_conflict_key(&txn_b);
        assert!(key_a.parallel_safe);
        assert_ne!(key_a.key, key_b.key);
    }

    #[test]
    fn replace_table_serializes_by_sink_and_namespace() {
        let txn = CompactionTransaction::new(
            "sink.main".to_string(),
            "ns".to_string(),
            "schema".to_string(),
            WritePolicy::ReplaceTable,
            SinkWriteSemantics::IdempotentAtLeastOnce,
            Vec::new(),
            "a".to_string(),
        );
        let conflict = sink_conflict_key(&txn);
        assert!(!conflict.parallel_safe);
        assert!(conflict.key.contains("replace_table"));
    }
}
