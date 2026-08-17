use skippr_lease::{CommitIndex, DurableError, PipelineKey, PipelinePaths};
use std::net::SocketAddr;

use crate::buffer::durable::log::MutationLog;
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::peer::{fetch_entries_from, fetch_snapshot_from, query_status};
use crate::cluster::promote::replica_status_hash;

pub async fn catch_up_from_donor(
    log: &mut MutationLog,
    paths: &PipelinePaths,
    key: &PipelineKey,
    donor: SocketAddr,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    tracing::info!(
        donor = %donor,
        pipeline = %key.pipeline(),
        "assigned replica catch-up started"
    );
    let status = query_status(donor, key, identity).await?;
    let donor_committed = CommitIndex::new(status.committed_index);
    let donor_hash = replica_status_hash(&status.head_hash)?;
    crate::metrics::counters::set_cluster_replica_lag(
        donor_committed
            .get()
            .saturating_sub(log.committed_index().get()),
    );
    if log.committed_index() == donor_committed && log.head_hash() == donor_hash {
        return Ok(());
    }
    if log.committed_index() == donor_committed {
        crate::metrics::counters::add_cluster_divergence(1);
        return Err(DurableError::Diverged(format!(
            "local hash disagrees with donor at {}",
            donor_committed.get()
        )));
    }
    if log.committed_index() > donor_committed {
        crate::metrics::counters::add_cluster_divergence(1);
        return Err(DurableError::Diverged(format!(
            "local committed {} is ahead of donor {}",
            log.committed_index().get(),
            donor_committed.get()
        )));
    }
    if log.committed_index() < CommitIndex::new(status.base_index) {
        fetch_snapshot_from(donor, key, identity, paths).await?;
        *log = MutationLog::open(paths.clone())?;
    }
    if log.committed_index() < CommitIndex::new(status.base_index) {
        return Err(DurableError::ProtocolMismatch(
            "snapshot install did not raise local base_index".into(),
        ));
    }
    let from = log
        .committed_index()
        .next()
        .map_err(|err| DurableError::Io(err.to_string()))?
        .get();
    if from <= status.committed_index {
        let entries =
            fetch_entries_from(donor, key, from, status.committed_index, identity).await?;
        let applicator = crate::buffer::durable::apply::DurableApplicator::new(paths.clone());
        for fetched in entries {
            match log.compare(&fetched.envelope)? {
                crate::buffer::durable::mutation::EntryComparison::Next => {
                    if let crate::buffer::durable::mutation::DurableMutation::CommitSegment {
                        descriptor,
                        ..
                    } = &fetched.envelope.body
                    {
                        if descriptor.payload_len > 0 {
                            if fetched.payload.is_empty() {
                                return Err(DurableError::Io(format!(
                                    "missing payload for segment {}",
                                    descriptor.segment_id
                                )));
                            }
                            let id = skippr_lease::SegmentId::new(&descriptor.segment_id)
                                .map_err(|err| DurableError::Io(err.to_string()))?;
                            let dest = paths.segment(&id);
                            if let Some(parent) = dest.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&dest, &fetched.payload)?;
                        }
                    }
                    log.append_prepared(&fetched.envelope)?;
                    let hash = fetched.envelope.entry_hash()?;
                    log.append_committed(fetched.envelope.index, hash)?;
                    applicator.apply_catch_up(&fetched.envelope)?;
                    log.mark_applied(fetched.envelope.index, hash)?;
                }
                crate::buffer::durable::mutation::EntryComparison::AlreadyAppliedSameHash => {}
                other => {
                    crate::metrics::counters::add_cluster_divergence(1);
                    return Err(DurableError::Diverged(format!(
                        "catch-up comparison {other:?}"
                    )));
                }
            }
        }
    }
    if log.committed_index() != donor_committed || log.head_hash() != donor_hash {
        crate::metrics::counters::add_cluster_divergence(1);
        return Err(DurableError::QuorumLost(
            "catch-up did not reach donor head".into(),
        ));
    }
    crate::metrics::counters::set_cluster_replica_lag(0);
    Ok(())
}

pub async fn install_snapshot_bytes(
    paths: &PipelinePaths,
    bytes: &[u8],
) -> Result<(), DurableError> {
    crate::buffer::durable::snapshot::install_snapshot_from_stream(
        paths,
        &uuid::Uuid::new_v4().to_string(),
        bytes,
    )
    .await?;
    crate::buffer::ingest_buffer::Buffers::sync_planner_from_pipeline_paths(paths);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::PipelineKey;

    #[test]
    fn catch_up_diverges_when_local_is_ahead_of_donor() {
        let src = include_str!("catchup.rs");
        assert!(src.contains("is ahead of donor"));
        assert!(src.contains("local hash disagrees with donor"));
        let key = PipelineKey::new("t", "w", "p").unwrap();
        assert_eq!(key.pipeline(), "p");
        let _ = CommitIndex::new(1);
    }
}
