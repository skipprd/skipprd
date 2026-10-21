use skippr_lease::{DurableError, PipelineKey, SegmentId};
use std::net::SocketAddr;
use std::path::Path;

use crate::buffer::wal_handle;
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::peer::{fetch_payload_from, query_status, ReplicaSession};
use skippr_wal_catalog::{query_all, CatalogPrefix, CatalogRow};

pub async fn catch_up_payloads(
    session: &ReplicaSession,
    donor: SocketAddr,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    tracing::info!(
        donor = %donor,
        pipeline = %session.key.pipeline(),
        "assigned replica payload catch-up started"
    );
    let _status = query_status(donor, &session.key, identity).await?;
    let Some(handle) = wal_handle::wal_handle_for(&session.key) else {
        return Err(DurableError::ProtocolMismatch(
            "replica catch-up requires a WAL catalog handle".into(),
        ));
    };
    fetch_missing_payloads(handle.as_ref(), &session.paths.segs, &[donor], identity).await?;
    crate::metrics::counters::set_cluster_replica_lag(0);
    tracing::info!(
        donor = %donor,
        pipeline = %session.key.pipeline(),
        "assigned replica payload catch-up complete"
    );
    Ok(())
}

pub async fn fetch_missing_payloads(
    handle: &crate::buffer::wal_handle::PipelineWalHandle,
    segs: &Path,
    donors: &[SocketAddr],
    identity: &ClusterIdentity,
) -> Result<Vec<String>, DurableError> {
    let rows = query_all(handle.catalog(), CatalogPrefix::Segment)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let mut missing = Vec::new();
    for (_, row) in rows {
        let CatalogRow::Segment(seg) = row else {
            continue;
        };
        let dest = segs.join(format!("{}.seg", seg.segment_id.as_str()));
        if dest.exists() {
            continue;
        }
        if donors.is_empty() {
            missing.push(seg.segment_id.as_str().to_string());
            continue;
        }
        let mut fetched = false;
        for donor in donors {
            if fetch_payload_from(
                *donor,
                handle.key(),
                seg.segment_id.as_str(),
                &dest,
                identity,
            )
            .await
            .is_ok()
            {
                fetched = true;
                break;
            }
        }
        if !fetched {
            missing.push(seg.segment_id.as_str().to_string());
        }
    }
    Ok(missing)
}

pub async fn catch_up_from_peers(
    key: &PipelineKey,
    segs: &Path,
    donors: &[SocketAddr],
    identity: &ClusterIdentity,
) -> Result<Vec<String>, DurableError> {
    let handle = wal_handle::wal_handle_for(key).ok_or_else(|| {
        DurableError::ProtocolMismatch("payload catch-up requires a WAL catalog handle".into())
    })?;
    fetch_missing_payloads(handle.as_ref(), segs, donors, identity).await
}

pub fn live_segment_id(id: &str) -> Result<SegmentId, DurableError> {
    SegmentId::new(id).map_err(|err| DurableError::Io(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_segment_id_rejects_empty() {
        assert!(live_segment_id("").is_err());
        assert!(live_segment_id("seg-1").is_ok());
    }
}
