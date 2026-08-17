use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use skippr_lease::{CommitIndex, DurableError, PipelinePaths, GENESIS_HASH};

use super::codec::{encode_envelope_proto, envelope_from_proto, proto};
use super::mutation::{EntryComparison, MutationEnvelope};
use prost::Message;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableState {
    pub base_index: CommitIndex,
    pub committed_index: CommitIndex,
    pub applied_index: CommitIndex,
    pub head_hash: [u8; 32],
}

impl Default for DurableState {
    fn default() -> Self {
        Self {
            base_index: CommitIndex::ZERO,
            committed_index: CommitIndex::ZERO,
            applied_index: CommitIndex::ZERO,
            head_hash: GENESIS_HASH,
        }
    }
}

pub struct MutationLog {
    paths: PipelinePaths,
    state: DurableState,
    pending_prepared: Vec<MutationEnvelope>,
    committed_envelopes: Vec<MutationEnvelope>,
}

impl MutationLog {
    pub fn open(paths: PipelinePaths) -> Result<Self, DurableError> {
        fs::create_dir_all(&paths.durable)?;
        fs::create_dir_all(&paths.snapshots)?;
        fs::create_dir_all(&paths.segs)?;
        let replayed = replay_log(&paths.mutation_log())?;
        let disk_state = read_state(&paths.state_file())?;
        if let Some(disk) = disk_state {
            if replayed.committed_envelopes.is_empty() && disk.committed_index > CommitIndex::ZERO {
                if disk.committed_index > disk.base_index {
                    return Err(DurableError::Io(
                        "STATE is ahead of mutation.log; refuse to apply".into(),
                    ));
                }
                return Ok(Self {
                    paths,
                    state: disk,
                    pending_prepared: Vec::new(),
                    committed_envelopes: Vec::new(),
                });
            }
        }
        let mut state = replayed.state;
        if let Some(disk) = disk_state {
            if disk.committed_index > state.committed_index {
                return Err(DurableError::Io(
                    "STATE is ahead of mutation.log; refuse to apply".into(),
                ));
            }
            if disk.applied_index > state.applied_index
                && disk.committed_index == state.committed_index
                && disk.head_hash == state.head_hash
            {
                state.applied_index = disk.applied_index;
                state.base_index = disk.base_index;
            }
        }
        Ok(Self {
            paths,
            state,
            pending_prepared: replayed.pending_prepared,
            committed_envelopes: replayed.committed_envelopes,
        })
    }

    pub fn committed_index(&self) -> CommitIndex {
        self.state.committed_index
    }

    pub fn applied_index(&self) -> CommitIndex {
        self.state.applied_index
    }

    pub fn base_index(&self) -> CommitIndex {
        self.state.base_index
    }

    pub fn head_hash(&self) -> [u8; 32] {
        self.state.head_hash
    }

    pub fn state(&self) -> DurableState {
        self.state
    }

    pub fn paths(&self) -> &PipelinePaths {
        &self.paths
    }

    pub fn pending_prepared(&self) -> &[MutationEnvelope] {
        &self.pending_prepared
    }

    pub fn committed_envelopes(&self) -> &[MutationEnvelope] {
        &self.committed_envelopes
    }

    pub fn envelope_at(&self, index: CommitIndex) -> Option<&MutationEnvelope> {
        self.committed_envelopes
            .iter()
            .find(|envelope| envelope.index == index)
    }

    pub fn compare(&self, envelope: &MutationEnvelope) -> Result<EntryComparison, DurableError> {
        let next = self
            .state
            .committed_index
            .next()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        if envelope.index == next {
            if envelope.previous_hash == self.state.head_hash {
                Ok(EntryComparison::Next)
            } else {
                Err(DurableError::Diverged("previous hash mismatch".into()))
            }
        } else if envelope.index <= self.state.committed_index {
            let hash = envelope.entry_hash()?;
            if envelope.index == self.state.committed_index {
                if hash == self.state.head_hash {
                    Ok(EntryComparison::AlreadyAppliedSameHash)
                } else {
                    Ok(EntryComparison::SameIndexDifferentHash)
                }
            } else if let Some(existing) = self.envelope_at(envelope.index) {
                if existing.entry_hash()? == hash {
                    Ok(EntryComparison::AlreadyAppliedSameHash)
                } else {
                    Ok(EntryComparison::SameIndexDifferentHash)
                }
            } else if envelope.index <= self.state.base_index {
                Ok(EntryComparison::AlreadyAppliedSameHash)
            } else {
                Err(DurableError::Diverged(format!(
                    "missing historical envelope at index {}",
                    envelope.index.get()
                )))
            }
        } else {
            Ok(EntryComparison::Gap {
                head: self.state.committed_index,
            })
        }
    }

    pub fn append_prepared(&mut self, envelope: &MutationEnvelope) -> Result<(), DurableError> {
        append_prepared(&self.paths.mutation_log(), envelope)?;
        self.pending_prepared.push(envelope.clone());
        Ok(())
    }

    pub fn append_committed(
        &mut self,
        index: CommitIndex,
        entry_hash: [u8; 32],
    ) -> Result<(), DurableError> {
        append_committed_marker(&self.paths.mutation_log(), index, entry_hash)?;
        if let Some(pos) = self.pending_prepared.iter().position(|envelope| {
            envelope.index == index && envelope.entry_hash().ok() == Some(entry_hash)
        }) {
            let envelope = self.pending_prepared.remove(pos);
            self.committed_envelopes.push(envelope);
        }
        self.state.committed_index = index;
        self.state.head_hash = entry_hash;
        crate::metrics::counters::set_cluster_committed_index(index.get());
        write_state_atomic(&self.paths.durable, &self.paths.state_file(), &self.state)
    }

    pub fn abort_pending_prepared(&mut self) -> Result<(), DurableError> {
        if self.pending_prepared.is_empty() {
            return Ok(());
        }
        rewrite_committed_log(&self.paths.mutation_log(), &self.committed_envelopes)?;
        self.pending_prepared.clear();
        Ok(())
    }

    pub fn mark_applied(
        &mut self,
        index: CommitIndex,
        entry_hash: [u8; 32],
    ) -> Result<(), DurableError> {
        if entry_hash != self.state.head_hash && index == self.state.committed_index {
            return Err(DurableError::Diverged(
                "applied hash does not match committed head".into(),
            ));
        }
        self.state.applied_index = index;
        crate::metrics::counters::set_cluster_applied_index(index.get());
        write_state_atomic(&self.paths.durable, &self.paths.state_file(), &self.state)
    }

    pub fn mark_offsets_published(&self, index: CommitIndex) -> Result<(), DurableError> {
        crate::metrics::counters::set_cluster_published_index(index.get());
        let path = self.paths.offset_published_file();
        atomic_write(&self.paths.durable, &path, &index.get().to_le_bytes())
    }

    pub fn prune_through(&mut self, base: CommitIndex) -> Result<(), DurableError> {
        self.committed_envelopes
            .retain(|envelope| envelope.index > base);
        self.state.base_index = base;
        rewrite_committed_log(&self.paths.mutation_log(), &self.committed_envelopes)?;
        write_state_atomic(&self.paths.durable, &self.paths.state_file(), &self.state)
    }

    pub fn envelopes_in_range(
        &self,
        from: u64,
        to: u64,
    ) -> impl Iterator<Item = &MutationEnvelope> {
        self.committed_envelopes
            .iter()
            .filter(move |envelope| envelope.index.get() >= from && envelope.index.get() <= to)
    }

    pub fn write_format_marker(&self) -> Result<(), DurableError> {
        atomic_write(
            &self.paths.durable,
            &self.paths.format_marker(),
            crate::buffer::durable::mutation::CLUSTER_FORMAT_V1.as_bytes(),
        )
    }

    pub fn has_format_marker(&self) -> bool {
        self.paths.format_marker().exists()
    }
}

struct Replayed {
    state: DurableState,
    pending_prepared: Vec<MutationEnvelope>,
    committed_envelopes: Vec<MutationEnvelope>,
}

fn replay_log(path: &Path) -> Result<Replayed, DurableError> {
    let mut state = DurableState::default();
    let mut pending: Vec<MutationEnvelope> = Vec::new();
    let mut committed = Vec::new();
    if !path.exists() {
        return Ok(Replayed {
            state,
            pending_prepared: pending,
            committed_envelopes: committed,
        });
    }
    let bytes = fs::read(path)?;
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let start = offset + 4;
        let end = start + len;
        if end + 32 > bytes.len() {
            break;
        }
        let payload = &bytes[start..end];
        let checksum = &bytes[end..end + 32];
        let mut hasher = Sha256::new();
        hasher.update(payload);
        let actual: [u8; 32] = hasher.finalize().into();
        if actual.as_slice() != checksum {
            break;
        }
        offset = end + 32;
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => {
                let proto = proto::MutationEnvelope::decode(&payload[1..]).map_err(|err| {
                    DurableError::ProtocolMismatch(format!("corrupt prepared envelope: {err}"))
                })?;
                pending.push(envelope_from_proto(proto)?);
            }
            2 => {
                if payload.len() < 1 + 8 + 32 {
                    break;
                }
                let index = CommitIndex::new(u64::from_le_bytes(payload[1..9].try_into().unwrap()));
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&payload[9..41]);
                if let Some(pos) = pending.iter().position(|envelope| {
                    envelope.index == index && envelope.entry_hash().ok() == Some(hash)
                }) {
                    let envelope = pending.remove(pos);
                    committed.push(envelope);
                }
                state.committed_index = index;
                state.head_hash = hash;
            }
            _ => break,
        }
    }
    Ok(Replayed {
        state,
        pending_prepared: pending,
        committed_envelopes: committed,
    })
}

fn rewrite_committed_log(path: &Path, envelopes: &[MutationEnvelope]) -> Result<(), DurableError> {
    let tmp = path.with_extension("rewrite");
    let _ = fs::remove_file(&tmp);
    for envelope in envelopes {
        append_prepared(&tmp, envelope)?;
        append_committed_marker(&tmp, envelope.index, envelope.entry_hash()?)?;
    }
    if tmp.exists() {
        fs::rename(&tmp, path)?;
    } else {
        fs::write(path, [])?;
    }
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn append_prepared(path: &Path, envelope: &MutationEnvelope) -> Result<(), DurableError> {
    let proto_bytes = encode_envelope_proto(envelope)?;
    let mut payload = Vec::with_capacity(1 + proto_bytes.len());
    payload.push(1);
    payload.extend_from_slice(&proto_bytes);
    write_framed(path, &payload)
}

fn append_committed_marker(
    path: &Path,
    index: CommitIndex,
    entry_hash: [u8; 32],
) -> Result<(), DurableError> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.extend_from_slice(&index.get().to_le_bytes());
    payload.extend_from_slice(&entry_hash);
    write_framed(path, &payload)
}

fn write_framed(path: &Path, payload: &[u8]) -> Result<(), DurableError> {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let checksum: [u8; 32] = hasher.finalize().into();
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(payload)?;
    file.write_all(&checksum)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<Option<DurableState>, DurableError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    if bytes.len() != 8 * 3 + 32 {
        return Err(DurableError::Io("corrupt STATE file".into()));
    }
    let base = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let committed = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let applied = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[24..56]);
    Ok(Some(DurableState {
        base_index: CommitIndex::new(base),
        committed_index: CommitIndex::new(committed),
        applied_index: CommitIndex::new(applied),
        head_hash: hash,
    }))
}

fn write_state_atomic(dir: &Path, path: &Path, state: &DurableState) -> Result<(), DurableError> {
    let mut bytes = Vec::with_capacity(56);
    bytes.extend_from_slice(&state.base_index.get().to_le_bytes());
    bytes.extend_from_slice(&state.committed_index.get().to_le_bytes());
    bytes.extend_from_slice(&state.applied_index.get().to_le_bytes());
    bytes.extend_from_slice(&state.head_hash);
    atomic_write(dir, path, &bytes)
}

fn atomic_write(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), DurableError> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(dir)?;
    Ok(())
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::durable::mutation::DurableMutation;
    use skippr_lease::{LeaseEpoch, PipelineKey, PipelinePaths};

    #[test]
    fn prepared_then_committed_advances_head_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [9u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        let hash = envelope.entry_hash().unwrap();
        log.append_committed(envelope.index, hash).unwrap();
        assert_eq!(log.committed_index(), CommitIndex::new(1));
        assert_eq!(log.head_hash(), hash);

        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.committed_index(), CommitIndex::new(1));
        assert_eq!(reopened.head_hash(), hash);
        assert!(reopened.pending_prepared().is_empty());
        assert_eq!(reopened.committed_envelopes().len(), 1);
    }

    #[test]
    fn truncated_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [1u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        log.append_committed(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(paths.mutation_log())
                .unwrap();
            file.write_all(&[4, 0, 0, 0, 1, 2, 3]).unwrap();
        }
        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.committed_index(), CommitIndex::new(1));
    }

    #[test]
    fn abort_pending_prepared_drops_uncommitted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [1u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        assert_eq!(log.pending_prepared().len(), 1);
        log.abort_pending_prepared().unwrap();
        assert!(log.pending_prepared().is_empty());
        let reopened = MutationLog::open(paths).unwrap();
        assert!(reopened.pending_prepared().is_empty());
        assert_eq!(reopened.committed_index(), CommitIndex::ZERO);
    }

    #[test]
    fn prune_through_rewrites_log_and_reopens_at_base() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [3u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        log.append_committed(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
        log.mark_applied(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
        log.prune_through(CommitIndex::new(1)).unwrap();
        assert!(log.committed_envelopes().is_empty());
        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.committed_index(), CommitIndex::new(1));
        assert_eq!(reopened.base_index(), CommitIndex::new(1));
        assert!(reopened.committed_envelopes().is_empty());
    }

    #[test]
    fn empty_log_state_ahead_of_base_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [3u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        log.append_committed(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
        log.mark_applied(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
        log.prune_through(CommitIndex::new(1)).unwrap();
        let state_path = paths.state_file();
        let mut raw = fs::read(&state_path).unwrap();
        let committed = u64::from_le_bytes(raw[8..16].try_into().unwrap()) + 1;
        raw[8..16].copy_from_slice(&committed.to_le_bytes());
        fs::write(&state_path, raw).unwrap();
        let err = match MutationLog::open(paths) {
            Ok(_) => panic!("expected STATE-ahead refusal"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("STATE is ahead of mutation.log"));
    }

    #[test]
    fn missing_suffix_envelope_is_diverged() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths).unwrap();
        let second = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(2),
            previous_hash: GENESIS_HASH,
            payload_sha256: [2u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "b".into(),
            },
        };
        log.append_prepared(&second).unwrap();
        log.append_committed(second.index, second.entry_hash().unwrap())
            .unwrap();
        let missing = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [1u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "a".into(),
            },
        };
        let err = log.compare(&missing).unwrap_err();
        assert!(err.to_string().contains("missing historical envelope"));
    }
}
