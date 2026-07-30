use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::buffer::compaction_transaction::{CompactionTransaction, SinkWriteSemantics};
use crate::plugins::source_contract::WritePolicy;

static ACTIVE_SINK_CONFLICT_KEYS: Lazy<DashMap<String, ()>> = Lazy::new(DashMap::new);
static EXACT_ONCE_COMMIT_LANES: Lazy<DashMap<String, Arc<Semaphore>>> = Lazy::new(DashMap::new);

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

/// Serialize only the final-state commit lane for exact-once writes. Callers
/// acquire this after WAL decode/build so read and encoding work stays parallel.
pub async fn acquire_exact_once_commit_lane(
    txn: &CompactionTransaction,
) -> Option<OwnedSemaphorePermit> {
    if txn.semantics != SinkWriteSemantics::ExactOnce {
        return None;
    }
    let key = format!("{}:{}", txn.sink_ref, txn.namespace);
    let lane = EXACT_ONCE_COMMIT_LANES
        .entry(key)
        .or_insert_with(|| Arc::new(Semaphore::new(1)))
        .clone();
    lane.acquire_owned().await.ok()
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
    use crate::buffer::compaction_transaction::CompactionTransaction;
    use std::time::Duration;

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

    #[tokio::test]
    async fn exact_once_commit_lane_serializes_namespace_only() {
        let txn_a = CompactionTransaction::new(
            "sink.exact-lane-test".to_string(),
            "ns-a".to_string(),
            "schema".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            Vec::new(),
            "a".to_string(),
        );
        let txn_same_namespace = txn_a.clone();
        let mut txn_other_namespace = txn_a.clone();
        txn_other_namespace.namespace = "ns-b".to_string();

        let first = acquire_exact_once_commit_lane(&txn_a).await.unwrap();
        let same_waiter = acquire_exact_once_commit_lane(&txn_same_namespace);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), same_waiter)
                .await
                .is_err(),
            "same exact-once namespace must wait for the active commit"
        );
        let other = tokio::time::timeout(
            Duration::from_millis(10),
            acquire_exact_once_commit_lane(&txn_other_namespace),
        )
        .await
        .expect("another namespace should proceed")
        .expect("exact-once lane");
        drop(other);
        drop(first);
        let _released_lane = tokio::time::timeout(
            Duration::from_millis(100),
            acquire_exact_once_commit_lane(&txn_same_namespace),
        )
        .await
        .expect("same namespace should proceed after release")
        .expect("exact-once lane");
    }
}
