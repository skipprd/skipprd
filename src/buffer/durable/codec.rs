use prost::Message;
use skippr_lease::{CommitIndex, DurableError, LeaseEpoch, PipelineKey, CONTROL_FRAME_MAX_BYTES};

use super::mutation::{
    CommittedCheckpoint, CommittedOffset, CompletedOrdinals, DurableMutation, MutationEnvelope,
    SegmentDescriptor,
};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/skippr.cluster.v1.rs"));
}

pub fn encode_control(frame: &proto::ControlFrame) -> Result<Vec<u8>, DurableError> {
    let mut buf = Vec::new();
    frame
        .encode(&mut buf)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
    if buf.len() > CONTROL_FRAME_MAX_BYTES {
        return Err(DurableError::ProtocolMismatch(
            "control frame exceeds 16 MiB".into(),
        ));
    }
    Ok(buf)
}

pub fn decode_control(bytes: &[u8]) -> Result<proto::ControlFrame, DurableError> {
    proto::ControlFrame::decode(bytes)
        .map_err(|err| DurableError::ProtocolMismatch(format!("corrupt protobuf: {err}")))
}

pub fn encode_envelope_proto(envelope: &MutationEnvelope) -> Result<Vec<u8>, DurableError> {
    let proto = envelope_to_proto(envelope)?;
    let mut buf = Vec::new();
    proto
        .encode(&mut buf)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
    Ok(buf)
}

pub fn envelope_to_proto(
    envelope: &MutationEnvelope,
) -> Result<proto::MutationEnvelope, DurableError> {
    Ok(proto::MutationEnvelope {
        protocol_version: envelope.protocol_version,
        pipeline: Some(pipeline_to_proto(&envelope.pipeline)),
        epoch: envelope.epoch.get(),
        commit_index: envelope.index.get(),
        previous_hash: envelope.previous_hash.to_vec(),
        payload_sha256: envelope.payload_sha256.to_vec(),
        body: Some(mutation_to_proto(&envelope.body)?),
    })
}

pub fn envelope_from_proto(
    proto: proto::MutationEnvelope,
) -> Result<MutationEnvelope, DurableError> {
    let pipeline = pipeline_from_proto(proto.pipeline.as_ref())?;
    if proto.epoch == 0 {
        return Err(DurableError::ProtocolMismatch(
            "clustered replica protocol rejects epoch 0".into(),
        ));
    }
    let previous_hash = hash32(&proto.previous_hash)?;
    let payload_sha256 = hash32(&proto.payload_sha256)?;
    let body = mutation_from_proto(proto.body)?;
    Ok(MutationEnvelope {
        protocol_version: proto.protocol_version,
        pipeline,
        epoch: LeaseEpoch::new(proto.epoch),
        index: CommitIndex::new(proto.commit_index),
        previous_hash,
        payload_sha256,
        body,
    })
}

pub fn pipeline_to_proto(key: &PipelineKey) -> proto::PipelineKey {
    proto::PipelineKey {
        tenant: key.tenant().to_string(),
        workspace: key.workspace().to_string(),
        pipeline: key.pipeline().to_string(),
    }
}

pub fn pipeline_from_proto(key: Option<&proto::PipelineKey>) -> Result<PipelineKey, DurableError> {
    let key = key.ok_or_else(|| DurableError::ProtocolMismatch("missing pipeline key".into()))?;
    if key.tenant.is_empty() || key.workspace.is_empty() || key.pipeline.is_empty() {
        return Err(DurableError::ProtocolMismatch(
            "pipeline key components must not be empty".into(),
        ));
    }
    PipelineKey::new(&key.tenant, &key.workspace, &key.pipeline)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))
}

fn mutation_to_proto(
    body: &DurableMutation,
) -> Result<proto::mutation_envelope::Body, DurableError> {
    match body {
        DurableMutation::CommitSegment {
            descriptor,
            offsets,
            checkpoints,
        } => {
            let mut offsets = offsets.clone();
            offsets.sort_by(|a, b| (&a.namespace, &a.partition).cmp(&(&b.namespace, &b.partition)));
            let mut checkpoints = checkpoints.clone();
            checkpoints.sort_by(|a, b| a.logical_key.cmp(&b.logical_key));
            Ok(proto::mutation_envelope::Body::CommitSegment(
                proto::CommitSegment {
                    segment: Some(proto::SegmentDescriptor {
                        segment_id: descriptor.segment_id.clone(),
                        payload_len: descriptor.payload_len,
                        payload_sha256: descriptor.payload_sha256.to_vec(),
                        num_partitions: descriptor.num_partitions,
                        total_bytes: descriptor.total_bytes,
                        created_at_secs: descriptor.created_at_secs,
                        schema_fingerprints: descriptor.schema_fingerprints.clone(),
                    }),
                    offsets: offsets
                        .into_iter()
                        .map(|offset| proto::CommittedOffset {
                            namespace: offset.namespace,
                            partition: offset.partition,
                            filesize: offset.filesize,
                            position: offset.position,
                            closed: offset.closed,
                        })
                        .collect(),
                    checkpoints: checkpoints
                        .into_iter()
                        .map(|checkpoint| proto::CommittedCheckpoint {
                            logical_key: checkpoint.logical_key,
                            envelope: checkpoint.envelope,
                        })
                        .collect(),
                },
            ))
        }
        DurableMutation::PutCompaction { transaction } => {
            let json =
                serde_json::to_vec(transaction).map_err(|err| DurableError::Io(err.to_string()))?;
            Ok(proto::mutation_envelope::Body::PutCompaction(
                proto::PutCompaction {
                    transaction_json: json,
                },
            ))
        }
        DurableMutation::CompleteSlices {
            compaction_id,
            entries,
        } => Ok(proto::mutation_envelope::Body::CompleteSlices(
            proto::CompleteSlices {
                compaction_id: compaction_id.clone(),
                entries: entries
                    .iter()
                    .map(|entry| proto::CompletedOrdinals {
                        segment_id: entry.segment_id.clone(),
                        ordinals: entry.ordinals.clone(),
                    })
                    .collect(),
            },
        )),
        DurableMutation::ReclaimSegment { segment_id } => Ok(
            proto::mutation_envelope::Body::ReclaimSegment(proto::ReclaimSegment {
                segment_id: segment_id.clone(),
            }),
        ),
    }
}

fn mutation_from_proto(
    body: Option<proto::mutation_envelope::Body>,
) -> Result<DurableMutation, DurableError> {
    match body.ok_or_else(|| DurableError::ProtocolMismatch("missing mutation body".into()))? {
        proto::mutation_envelope::Body::CommitSegment(commit) => {
            let descriptor = commit.segment.ok_or_else(|| {
                DurableError::ProtocolMismatch("commit segment missing descriptor".into())
            })?;
            Ok(DurableMutation::CommitSegment {
                descriptor: SegmentDescriptor {
                    segment_id: descriptor.segment_id,
                    payload_len: descriptor.payload_len,
                    payload_sha256: hash32(&descriptor.payload_sha256)?,
                    num_partitions: descriptor.num_partitions,
                    total_bytes: descriptor.total_bytes,
                    created_at_secs: descriptor.created_at_secs,
                    schema_fingerprints: descriptor.schema_fingerprints,
                },
                offsets: commit
                    .offsets
                    .into_iter()
                    .map(|offset| CommittedOffset {
                        namespace: offset.namespace,
                        partition: offset.partition,
                        filesize: offset.filesize,
                        position: offset.position,
                        closed: offset.closed,
                    })
                    .collect(),
                checkpoints: commit
                    .checkpoints
                    .into_iter()
                    .map(|checkpoint| CommittedCheckpoint {
                        logical_key: checkpoint.logical_key,
                        envelope: checkpoint.envelope,
                    })
                    .collect(),
            })
        }
        proto::mutation_envelope::Body::PutCompaction(put) => {
            let transaction = serde_json::from_slice(&put.transaction_json)
                .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
            Ok(DurableMutation::PutCompaction { transaction })
        }
        proto::mutation_envelope::Body::CompleteSlices(complete) => {
            Ok(DurableMutation::CompleteSlices {
                compaction_id: complete.compaction_id,
                entries: complete
                    .entries
                    .into_iter()
                    .map(|entry| CompletedOrdinals {
                        segment_id: entry.segment_id,
                        ordinals: entry.ordinals,
                    })
                    .collect(),
            })
        }
        proto::mutation_envelope::Body::ReclaimSegment(reclaim) => {
            Ok(DurableMutation::ReclaimSegment {
                segment_id: reclaim.segment_id,
            })
        }
    }
}

pub fn hash32(bytes: &[u8]) -> Result<[u8; 32], DurableError> {
    if bytes.len() != 32 {
        return Err(DurableError::ProtocolMismatch(format!(
            "expected 32-byte hash, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use skippr_lease::GENESIS_HASH;

    #[test]
    fn envelope_round_trip_and_golden_hash() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [9u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "seg-1".into(),
            },
        };
        let proto_bytes = encode_envelope_proto(&envelope).unwrap();
        let decoded =
            envelope_from_proto(proto::MutationEnvelope::decode(&proto_bytes[..]).unwrap())
                .unwrap();
        assert_eq!(
            decoded.entry_hash().unwrap(),
            envelope.entry_hash().unwrap()
        );
        let hash = hex::encode(envelope.entry_hash().unwrap());
        assert_eq!(hash.len(), 64);
        assert_eq!(
            hash,
            "b4da9461d025c806de533dc398aaec7f5a0d12c7d199306ad0a9d3bba08e75bf"
        );
    }

    #[test]
    fn clustered_epoch_zero_is_rejected() {
        let err = envelope_from_proto(proto::MutationEnvelope {
            protocol_version: 1,
            pipeline: Some(proto::PipelineKey {
                tenant: "t".into(),
                workspace: "w".into(),
                pipeline: "p".into(),
            }),
            epoch: 0,
            commit_index: 1,
            previous_hash: GENESIS_HASH.to_vec(),
            payload_sha256: [9u8; 32].to_vec(),
            body: Some(proto::mutation_envelope::Body::ReclaimSegment(
                proto::ReclaimSegment {
                    segment_id: "seg-1".into(),
                },
            )),
        })
        .unwrap_err();
        assert!(matches!(err, DurableError::ProtocolMismatch(_)));
    }

    #[test]
    fn every_durable_mutation_round_trips() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let bodies = [
            DurableMutation::CommitSegment {
                descriptor: SegmentDescriptor {
                    segment_id: "seg-a".into(),
                    payload_len: 4,
                    payload_sha256: [1u8; 32],
                    num_partitions: 1,
                    total_bytes: 4,
                    created_at_secs: 1,
                    schema_fingerprints: vec!["fp".into()],
                },
                offsets: vec![CommittedOffset {
                    namespace: "ns".into(),
                    partition: "0".into(),
                    filesize: 1,
                    position: 2,
                    closed: 0,
                }],
                checkpoints: vec![CommittedCheckpoint {
                    logical_key: "ck".into(),
                    envelope: vec![1, 2, 3],
                }],
            },
            DurableMutation::CompleteSlices {
                compaction_id: "c1".into(),
                entries: vec![CompletedOrdinals {
                    segment_id: "seg-a".into(),
                    ordinals: vec![0, 1],
                }],
            },
            DurableMutation::ReclaimSegment {
                segment_id: "seg-a".into(),
            },
        ];
        for body in bodies {
            let envelope = MutationEnvelope {
                protocol_version: 1,
                pipeline: key.clone(),
                epoch: LeaseEpoch::new(1),
                index: CommitIndex::new(1),
                previous_hash: GENESIS_HASH,
                payload_sha256: [2u8; 32],
                body,
            };
            let proto_bytes = encode_envelope_proto(&envelope).unwrap();
            let decoded =
                envelope_from_proto(proto::MutationEnvelope::decode(&proto_bytes[..]).unwrap())
                    .unwrap();
            assert_eq!(
                decoded.entry_hash().unwrap(),
                envelope.entry_hash().unwrap()
            );
        }
    }

    #[test]
    fn empty_pipeline_key_is_rejected() {
        let err = pipeline_from_proto(Some(&proto::PipelineKey {
            tenant: String::new(),
            workspace: "w".into(),
            pipeline: "p".into(),
        }))
        .unwrap_err();
        assert!(matches!(err, DurableError::ProtocolMismatch(_)));
    }
}
