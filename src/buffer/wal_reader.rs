//! Read-only Skippr Store WAL index. Two prefix scans at pin time; fail closed.
//!
//! ```compile_fail
//! fn must_not_persist(reader: &skipprd::buffer::wal_reader::WalCatalogReader) {
//!     reader.persist_segment();
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use skippr_lease::{DurableError, PipelineKey, SegmentId};

/// Pinned WAL slice was reclaimed after plan. Retry the query.
#[derive(Debug)]
pub struct WalPinLost {
    pub segment_id: SegmentId,
    pub start: u64,
}

impl std::fmt::Display for WalPinLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WAL slice {}@{} was reclaimed after the query pinned it; retry the query",
            self.segment_id.as_str(),
            self.start
        )
    }
}

impl std::error::Error for WalPinLost {}

/// Exact Index/Segment lookup failed closed.
#[derive(Debug)]
pub enum PinReadError {
    Lost(WalPinLost),
    Durable(DurableError),
}

impl std::fmt::Display for PinReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lost(lost) => write!(f, "{lost}"),
            Self::Durable(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PinReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lost(lost) => Some(lost),
            Self::Durable(err) => Some(err),
        }
    }
}

impl From<DurableError> for PinReadError {
    fn from(err: DurableError) -> Self {
        Self::Durable(err)
    }
}
use skippr_wal_catalog::{
    query_all, CatalogError, CatalogKey, CatalogPrefix, CatalogRow, CatalogSeq, PayloadLocation,
    SegmentRow, SliceDirEntry, WalCatalogRead,
};

use crate::wal_slice_ref::WalSliceRef;

static QUERY_READERS: OnceLock<RwLock<HashMap<PipelineKey, WalCatalogReader>>> = OnceLock::new();

fn query_readers() -> &'static RwLock<HashMap<PipelineKey, WalCatalogReader>> {
    QUERY_READERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Same-process ingest already holds the Skippr Store; wrap it as a reader.
/// Otherwise use the query-opened `QUERY_READERS` entry. Live slices always
/// go through `WalCatalogReader::live_slices`.
pub fn wal_reader_for(key: &PipelineKey) -> Option<WalCatalogReader> {
    if let Some(handle) = crate::buffer::wal_handle::wal_handle_for(key) {
        return Some(handle.reader());
    }
    query_readers()
        .read()
        .expect("query WAL reader map lock poisoned")
        .get(key)
        .cloned()
}

pub fn install_query_reader(reader: WalCatalogReader) -> std::io::Result<()> {
    let mut map = query_readers()
        .write()
        .map_err(|_| std::io::Error::other("query WAL reader map lock poisoned"))?;
    map.insert(reader.pipeline().clone(), reader);
    Ok(())
}

#[cfg(test)]
pub fn clear_query_readers() {
    if let Ok(mut map) = query_readers().write() {
        map.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedSlice {
    pub seq: CatalogSeq,
    pub segment_id: SegmentId,
    pub location: PayloadLocation,
    pub payload_sha256: [u8; 32],
    pub entry: SliceDirEntry,
}

impl PinnedSlice {
    pub fn slice_ref(&self) -> WalSliceRef {
        WalSliceRef::new(self.segment_id.clone(), self.entry.start)
    }
}

#[derive(Clone)]
pub struct WalCatalogReader {
    key: PipelineKey,
    catalog: Arc<dyn WalCatalogRead>,
}

impl std::fmt::Debug for WalCatalogReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalCatalogReader")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl WalCatalogReader {
    pub fn new(key: PipelineKey, catalog: Arc<dyn WalCatalogRead>) -> Self {
        Self { key, catalog }
    }

    pub fn pipeline(&self) -> &PipelineKey {
        &self.key
    }

    /// Index then Segment prefix scans. An Index row without a Segment row was
    /// reclaimed between the scans and is skipped (Iceberg `E` covers it).
    pub fn live_slices(&self, namespace: &str) -> Result<Vec<PinnedSlice>, DurableError> {
        let mut out = Vec::new();
        for slice in self.live_entries()? {
            if slice.entry.namespace == namespace {
                out.push(slice);
            }
        }
        Ok(out)
    }

    pub fn live_namespaces(&self) -> Result<Vec<String>, DurableError> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        for slice in self.live_entries()? {
            let name = slice.entry.namespace;
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            names.push(name);
        }
        Ok(names)
    }

    /// Exact Index + Segment lookup. A miss is pin-lost, not a skip.
    pub fn pinned_slice(&self, segment_id: &str, start: u64) -> Result<PinnedSlice, PinReadError> {
        let segment_id = SegmentId::new(segment_id)
            .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        let index = match self
            .catalog
            .get(&CatalogKey::Index(segment_id.clone()))
            .map_err(catalog_err)?
        {
            Some(CatalogRow::Index(index)) => index,
            Some(_) => {
                return Err(DurableError::ProtocolMismatch(
                    "wal#idx# holds a non-index row".into(),
                )
                .into());
            }
            None => return Err(pin_lost(&segment_id, start)),
        };
        let seg = match self
            .catalog
            .get(&CatalogKey::Segment(segment_id.clone()))
            .map_err(catalog_err)?
        {
            Some(CatalogRow::Segment(seg)) => seg,
            Some(_) => {
                return Err(DurableError::ProtocolMismatch(
                    "wal#seg# holds a non-segment row".into(),
                )
                .into());
            }
            None => return Err(pin_lost(&segment_id, start)),
        };
        let entry = index
            .entries
            .into_iter()
            .find(|entry| entry.start == start)
            .ok_or_else(|| pin_lost(&segment_id, start))?;
        Ok(PinnedSlice {
            seq: seg.seq,
            segment_id,
            location: seg.location,
            payload_sha256: seg.payload_sha256,
            entry,
        })
    }

    fn live_entries(&self) -> Result<Vec<PinnedSlice>, DurableError> {
        let indexes =
            query_all(self.catalog.as_ref(), CatalogPrefix::Index).map_err(catalog_err)?;
        let segments: HashMap<SegmentId, SegmentRow> =
            query_all(self.catalog.as_ref(), CatalogPrefix::Segment)
                .map_err(catalog_err)?
                .into_iter()
                .map(|(_, row)| match row {
                    CatalogRow::Segment(seg) => Ok((seg.segment_id.clone(), seg)),
                    _ => Err(DurableError::ProtocolMismatch(
                        "wal#seg# holds a non-segment row".into(),
                    )),
                })
                .collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for (_, row) in indexes {
            let CatalogRow::Index(index) = row else {
                return Err(DurableError::ProtocolMismatch(
                    "wal#idx# holds a non-index row".into(),
                ));
            };
            let Some(seg) = segments.get(&index.segment_id) else {
                continue;
            };
            for entry in index.entries {
                out.push(PinnedSlice {
                    seq: seg.seq,
                    segment_id: index.segment_id.clone(),
                    location: seg.location.clone(),
                    payload_sha256: seg.payload_sha256,
                    entry,
                });
            }
        }
        out.sort_by_key(|slice| (slice.seq.get(), slice.entry.ordinal));
        Ok(out)
    }
}

fn catalog_err(err: CatalogError) -> DurableError {
    match err {
        CatalogError::Io(msg) => DurableError::Io(msg),
        other => DurableError::ProtocolMismatch(other.to_string()),
    }
}

fn pin_lost(segment_id: &SegmentId, start: u64) -> PinReadError {
    PinReadError::Lost(WalPinLost {
        segment_id: segment_id.clone(),
        start,
    })
}

#[cfg(test)]
pub(crate) fn reader_with_live_slice(
    key: PipelineKey,
    namespace: &str,
    segment: &str,
) -> WalCatalogReader {
    use skippr_lease::{LeaseEpoch, NodeId};
    use skippr_wal_catalog::{
        CatalogFence, CatalogKey, CatalogRow, CatalogTxn, MemoryWalCatalog, PutCond,
        SegmentIndexRow, SegmentRow, SliceBitmap, SliceDirEntry, WalCatalogStore,
    };

    let store = MemoryWalCatalog::new();
    let fence = CatalogFence {
        owner: NodeId::generate(),
        epoch: LeaseEpoch::new(1),
    };
    store.set_writer_fence(fence.owner, fence.epoch);
    let segment_id = SegmentId::new(segment).unwrap();
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Segment(segment_id.clone()),
        CatalogRow::Segment(SegmentRow {
            segment_id: segment_id.clone(),
            seq: CatalogSeq::new(1),
            location: PayloadLocation::Disk {
                path: format!("/tmp/{segment}.seg"),
            },
            payload_len: 1,
            payload_sha256: [0; 32],
            slice_count: 1,
            done: SliceBitmap::new(1),
            offsets: Vec::new(),
            checkpoints: Vec::new(),
            epoch: LeaseEpoch::new(1),
            version: 0,
        }),
        PutCond::NotExists,
    )
    .unwrap();
    txn.put(
        CatalogKey::Index(segment_id.clone()),
        CatalogRow::Index(SegmentIndexRow {
            segment_id,
            version: 0,
            entries: vec![SliceDirEntry {
                ordinal: 0,
                namespace: namespace.into(),
                partition: "p0".into(),
                event_time: None,
                start: 0,
                len: 10,
                codec: crate::buffer::segment_file::PART_CODEC_ARROW,
                row_count: Some(1),
                sink_ref: "data_sinks.iceberg".into(),
                schema_fingerprint: "fp".into(),
                cdc_meta_hash: None,
                part_meta_start: 0,
                part_meta_len: 0,
            }],
        }),
        PutCond::NotExists,
    )
    .unwrap();
    store.commit(&txn, fence).unwrap();
    WalCatalogReader::new(key, Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{LeaseEpoch, NodeId};
    use skippr_wal_catalog::{
        CatalogFence, CatalogKey, CatalogRow, CatalogTxn, MemoryWalCatalog, PutCond,
        SegmentIndexRow, SegmentRow, SliceBitmap, WalCatalogStore,
    };

    fn fence() -> CatalogFence {
        CatalogFence {
            owner: NodeId::generate(),
            epoch: LeaseEpoch::new(1),
        }
    }

    fn entry(namespace: &str, partition: &str, ordinal: u32, start: u64) -> SliceDirEntry {
        SliceDirEntry {
            ordinal,
            namespace: namespace.into(),
            partition: partition.into(),
            event_time: None,
            start,
            len: 10,
            codec: 1,
            row_count: Some(1),
            sink_ref: "data_sinks.iceberg".into(),
            schema_fingerprint: "fp".into(),
            cdc_meta_hash: None,
            part_meta_start: 0,
            part_meta_len: 0,
        }
    }

    fn put_segment(
        store: &MemoryWalCatalog,
        fence: CatalogFence,
        segment: &str,
        seq: u64,
        entries: Vec<SliceDirEntry>,
    ) {
        let segment_id = SegmentId::new(segment).unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Segment(segment_id.clone()),
            CatalogRow::Segment(SegmentRow {
                segment_id: segment_id.clone(),
                seq: CatalogSeq::new(seq),
                location: PayloadLocation::Disk {
                    path: format!("/tmp/{segment}.seg"),
                },
                payload_len: 1,
                payload_sha256: [0; 32],
                slice_count: entries.len() as u32,
                done: SliceBitmap::new(entries.len() as u32),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
                epoch: LeaseEpoch::new(1),
                version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        txn.put(
            CatalogKey::Index(segment_id.clone()),
            CatalogRow::Index(SegmentIndexRow {
                segment_id,
                version: 0,
                entries,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        store.commit(&txn, fence).unwrap();
    }

    fn reader(store: MemoryWalCatalog) -> WalCatalogReader {
        WalCatalogReader::new(PipelineKey::new("t", "w", "p").unwrap(), Arc::new(store))
    }

    struct FailingRead {
        inner: MemoryWalCatalog,
    }

    impl WalCatalogRead for FailingRead {
        fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
            self.inner.get(key)
        }

        fn query_prefix(
            &self,
            prefix: CatalogPrefix,
            after: Option<&CatalogKey>,
            limit: usize,
        ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
            let _ = (prefix, after, limit);
            Err(CatalogError::Io("injected query failure".into()))
        }
    }

    #[test]
    fn live_slices_filters_namespace_and_orders_by_seq_ordinal() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        put_segment(
            &store,
            f,
            "seg-b",
            2,
            vec![entry("ns", "p0", 1, 20), entry("other", "p9", 0, 1)],
        );
        put_segment(&store, f, "seg-a", 1, vec![entry("ns", "p1", 0, 10)]);
        let live = reader(store).live_slices("ns").unwrap();
        assert_eq!(
            live.iter()
                .map(|s| (s.seq.get(), s.entry.ordinal, s.entry.partition.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, 0, "p1"), (2, 1, "p0")]
        );
    }

    #[test]
    fn live_slices_skips_index_without_segment_row() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        put_segment(&store, f, "live", 1, vec![entry("ns", "p0", 0, 0)]);
        let gone = SegmentId::new("gone").unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Index(gone.clone()),
            CatalogRow::Index(SegmentIndexRow {
                segment_id: gone,
                version: 0,
                entries: vec![entry("ns", "p-gone", 0, 0)],
            }),
            PutCond::NotExists,
        )
        .unwrap();
        store.commit(&txn, f).unwrap();
        let live = reader(store).live_slices("ns").unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].segment_id.as_str(), "live");
    }

    #[test]
    fn live_slices_returns_query_prefix_error() {
        let inner = MemoryWalCatalog::new();
        let reader = WalCatalogReader::new(
            PipelineKey::new("t", "w", "p").unwrap(),
            Arc::new(FailingRead { inner }),
        );
        let err = reader.live_slices("ns").unwrap_err();
        assert!(
            matches!(err, DurableError::Io(ref msg) if msg.contains("injected query failure")),
            "catalog query failure must not become an empty live set, got {err}"
        );
    }

    #[test]
    fn live_slices_rejects_wrong_typed_index_row() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        let segment_id = SegmentId::new("bad").unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Index(segment_id.clone()),
            CatalogRow::Segment(SegmentRow {
                segment_id,
                seq: CatalogSeq::new(1),
                location: PayloadLocation::Disk {
                    path: "/tmp/bad.seg".into(),
                },
                payload_len: 1,
                payload_sha256: [0; 32],
                slice_count: 0,
                done: SliceBitmap::new(0),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
                epoch: LeaseEpoch::new(1),
                version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        store.commit(&txn, f).unwrap();
        let err = reader(store).live_slices("ns").unwrap_err();
        assert!(
            matches!(err, DurableError::ProtocolMismatch(ref msg) if msg.contains("wal#idx#")),
            "got {err}"
        );
    }

    #[test]
    fn live_namespaces_come_from_index_rows() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        put_segment(
            &store,
            f,
            "seg-a",
            1,
            vec![entry("ns", "p0", 0, 0), entry("other", "p1", 1, 10)],
        );
        let mut names = reader(store).live_namespaces().unwrap();
        names.sort();
        assert_eq!(names, vec!["ns".to_string(), "other".to_string()]);
    }

    #[test]
    fn live_slices_does_not_stat_payload() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        put_segment(&store, f, "ghost", 1, vec![entry("ns", "p0", 0, 0)]);
        let live = reader(store).live_slices("ns").unwrap();
        assert_eq!(live.len(), 1);
        let PayloadLocation::Disk { path } = &live[0].location else {
            panic!("expected disk location");
        };
        assert!(
            !std::path::Path::new(path).exists(),
            "pin must not skip a missing payload file"
        );
    }

    #[test]
    fn pinned_slice_is_exact_index_and_segment_get() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        put_segment(&store, f, "seg-a", 4, vec![entry("ns", "p0", 0, 10)]);
        let reader = reader(store);
        let slice = reader.pinned_slice("seg-a", 10).unwrap();
        assert_eq!(slice.segment_id.as_str(), "seg-a");
        assert_eq!(slice.entry.start, 10);
        assert_eq!(slice.seq.get(), 4);
        let miss = reader.pinned_slice("seg-a", 99).unwrap_err();
        assert!(
            matches!(&miss, PinReadError::Lost(lost) if lost.start == 99),
            "{miss}"
        );
        let gone = reader.pinned_slice("missing", 0).unwrap_err();
        assert!(
            matches!(&gone, PinReadError::Lost(lost) if lost.segment_id.as_str() == "missing"),
            "{gone}"
        );
    }

    #[test]
    fn reader_has_no_persist_or_complete_methods() {
        let src = include_str!("wal_reader.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod.contains("fn persist"),
            "WalCatalogReader must not persist"
        );
        assert!(
            !prod.contains("fn complete"),
            "WalCatalogReader must not complete slices"
        );
        assert!(
            !prod.contains("payload_present"),
            "pin must not stat payload"
        );
    }
}
