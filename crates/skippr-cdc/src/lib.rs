//! One winner test. Callers do not compare tokens themselves.

/// Iceberg side-file shard count. Query and the writer share this bound.
pub const ICEBERG_CDC_STATE_SHARDS: u8 = 64;

/// WAL address tie. A later `(seq, ordinal, row)` wins when tokens are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tie {
    pub seq: u64,
    pub ordinal: u32,
    pub row: u64,
}

/// Committed winner. The tie is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWinner {
    pub order: Vec<u8>,
    pub tie: Tie,
}

/// Length-prefixed column name then value. Query and Iceberg share this.
pub fn encode_business_key(parts: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in parts {
        write_len(&mut out, name.len());
        out.extend_from_slice(name);
        write_len(&mut out, value.len());
        out.extend_from_slice(value);
    }
    out
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u32).to_be_bytes());
}

/// Per-key reduction. A delete of a never-stored key is still a tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collapse {
    Upsert(StoredWinner),
    Tombstone { key: Vec<u8>, winner: StoredWinner },
}

/// Fold `incoming` against a stored incumbent. `incoming_delete` is the
/// candidate's mutation. `incumbent_tombstone` is whether the stored winner
/// was a tombstone. The incumbent is already `winner_of_set` of row and tomb.
pub fn collapse(
    incoming: &StoredWinner,
    incoming_delete: bool,
    incumbent: Option<&StoredWinner>,
    incumbent_tombstone: bool,
    key: Vec<u8>,
) -> Collapse {
    match incumbent {
        Some(inc) if !beats(incoming, inc) => {
            if incumbent_tombstone {
                Collapse::Tombstone {
                    key,
                    winner: inc.clone(),
                }
            } else {
                Collapse::Upsert(inc.clone())
            }
        }
        _ if incoming_delete => Collapse::Tombstone {
            key,
            winner: incoming.clone(),
        },
        _ => Collapse::Upsert(incoming.clone()),
    }
}

/// `candidate` replaces `incumbent` when its token is greater, or the tokens
/// match and its tie is later. An equal token and equal tie does not replace.
pub fn beats(candidate: &StoredWinner, incumbent: &StoredWinner) -> bool {
    match candidate.order.cmp(&incumbent.order) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => candidate.tie > incumbent.tie,
    }
}

/// Row slot vs tombstone slot with no incoming event. A strictly stronger
/// tombstone is the incumbent. Equal winners keep the row.
pub fn stored_incumbent(
    row: Option<&StoredWinner>,
    tombstone: Option<&StoredWinner>,
) -> Option<(StoredWinner, bool)> {
    match (row, tombstone) {
        (None, None) => None,
        (Some(row), None) => Some((row.clone(), false)),
        (None, Some(tombstone)) => Some((tombstone.clone(), true)),
        (Some(row), Some(tombstone)) => {
            if beats(tombstone, row) {
                Some((tombstone.clone(), true))
            } else {
                Some((row.clone(), false))
            }
        }
    }
}

/// Winner of the row slot, the tombstone slot, and the incoming event.
/// Both stored slots take this winner. Row 10, tombstone 4, delete 7 stays 10.
pub fn winner_of_set(
    row: Option<&StoredWinner>,
    tombstone: Option<&StoredWinner>,
    incoming: &StoredWinner,
) -> StoredWinner {
    let mut winner = incoming.clone();
    if let Some(tombstone) = tombstone {
        if !beats(&winner, tombstone) {
            winner = tombstone.clone();
        }
    }
    if let Some(row) = row {
        if !beats(&winner, row) {
            winner = row.clone();
        }
    }
    winner
}

/// Insert half of a primary-key split. The delete half keeps `parent`.
pub fn derived_insert_event_id(parent: &[u8]) -> Vec<u8> {
    let mut id = parent.to_vec();
    id.extend_from_slice(b"\0insert");
    id
}

/// Fixed-width hex tokens. Equal width is required. The incoming token replaces
/// the seen token only when [`beats`] says so. Ties are equal here because the
/// Iceberg shard does not yet store one; an equal token does not replace.
pub fn incoming_replaces_fixed_width(seen: &str, incoming: &str) -> Result<bool, String> {
    validate_fixed_width_hex(seen)?;
    validate_fixed_width_hex(incoming)?;
    if seen.len() != incoming.len() {
        return Err(format!(
            "CDC order token width changed from {} to {}",
            seen.len(),
            incoming.len()
        ));
    }
    let seen = stored(seen.as_bytes(), 0);
    let incoming = stored(incoming.as_bytes(), 0);
    Ok(beats(&incoming, &seen))
}

fn validate_fixed_width_hex(token: &str) -> Result<(), String> {
    if token.is_empty() || token.len() % 2 != 0 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("CDC order token '{token}' is not fixed-width hex"));
    }
    Ok(())
}

/// Which id a range pin must not split. A source transaction id wins when the
/// plugin sent one. Otherwise the ingest batch id is the boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RangeBoundary {
    Source(Vec<u8>),
    Ingest(u64),
}

pub fn range_boundary(source_transaction_id: Option<&[u8]>, ingest_batch_id: u64) -> RangeBoundary {
    match source_transaction_id {
        Some(id) => RangeBoundary::Source(id.to_vec()),
        None => RangeBoundary::Ingest(ingest_batch_id),
    }
}

#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub seq: u64,
    pub boundaries: Vec<RangeBoundary>,
}

/// Assign the next sequence number. Callers serialize this so Iceberg and
/// warehouse publishes cannot share a seq.
pub fn append_entry(entries: &mut Vec<CatalogEntry>, boundaries: Vec<RangeBoundary>) -> u64 {
    let seq = entries.iter().map(|entry| entry.seq).max().unwrap_or(0) + 1;
    entries.push(CatalogEntry { seq, boundaries });
    seq
}

/// Slide a start entry back to the earliest first entry of every boundary id
/// that entry holds. The tip is not moved.
pub fn align_start(entries: &[CatalogEntry], chosen: u64) -> u64 {
    let Some(start) = entries.iter().find(|entry| entry.seq == chosen) else {
        return chosen;
    };
    start
        .boundaries
        .iter()
        .map(|boundary| {
            entries
                .iter()
                .filter(|entry| entry.boundaries.iter().any(|id| id == boundary))
                .map(|entry| entry.seq)
                .min()
                .unwrap_or(chosen)
        })
        .min()
        .unwrap_or(chosen)
}

/// Lowest cursor of the running computes. No compute means compaction is free.
pub fn global_min(cursors: &[u64]) -> Option<u64> {
    cursors.iter().copied().min()
}

/// A segment may be removed when no compute is running, or the global minimum
/// is already past that segment.
pub fn compaction_may_remove(minimum: Option<u64>, segment_seq: u64) -> bool {
    match minimum {
        None => true,
        Some(minimum) => segment_seq < minimum,
    }
}

fn stored(order: &[u8], row: u64) -> StoredWinner {
    StoredWinner {
        order: order.to_vec(),
        tie: Tie {
            seq: 1,
            ordinal: 0,
            row,
        },
    }
}

/// Every rule in the CDC one model that is a pure function. DST and the crate
/// tests both call this, so the matrix cannot drift.
pub fn prove_matrix() -> Result<(), String> {
    let earlier = stored(b"\x07", 1);
    let later = stored(b"\x07", 2);
    if !beats(&later, &earlier) || beats(&earlier, &later) || beats(&earlier, &earlier) {
        return Err("equal token: later tie wins, equal tie does not replace".into());
    }
    let low = stored(b"\x04", 99);
    let high = stored(b"\x0a", 0);
    if !beats(&high, &low) || beats(&low, &high) {
        return Err("greater token wins".into());
    }
    let row = stored(b"\x0a", 1);
    let tomb = stored(b"\x04", 1);
    let delete = stored(b"\x07", 1);
    if winner_of_set(Some(&row), Some(&tomb), &delete) != row {
        return Err("row 10, tombstone 4, delete 7 keeps the row".into());
    }
    if winner_of_set(None, Some(&tomb), &delete) != delete {
        return Err("delete 7 beats tombstone 4 when no row is stored".into());
    }
    let tomb_high = stored(b"\x0a", 1);
    let row_low = stored(b"\x04", 1);
    if winner_of_set(Some(&row_low), Some(&tomb_high), &delete) != tomb_high {
        return Err("tombstone 10 beats delete 7 and row 4".into());
    }
    match stored_incumbent(Some(&row), Some(&tomb)) {
        Some((winner, false)) if winner == row => {}
        _ => return Err("row 10 beats tombstone 4 as the stored incumbent".into()),
    }
    match stored_incumbent(None, Some(&tomb_high)) {
        Some((winner, true)) if winner == tomb_high => {}
        _ => return Err("a tombstone with no row is the stored incumbent".into()),
    }
    let parent = b"pg:1";
    let insert = derived_insert_event_id(parent);
    if insert == parent || !insert.starts_with(parent) || derived_insert_event_id(parent) != insert
    {
        return Err("insert event id is a stable suffix of the parent".into());
    }
    if incoming_replaces_fixed_width("0002", "0001").map_err(|e| e)? {
        return Err("a lesser hex token must not replace".into());
    }
    if !incoming_replaces_fixed_width("0001", "0002").map_err(|e| e)? {
        return Err("a greater hex token must replace".into());
    }
    if incoming_replaces_fixed_width("0001", "0001").map_err(|e| e)? {
        return Err("an equal hex token must not replace".into());
    }
    if incoming_replaces_fixed_width("02", "0001").is_ok()
        || incoming_replaces_fixed_width("", "00").is_ok()
        || incoming_replaces_fixed_width("zz", "00").is_ok()
    {
        return Err("width, empty, and non-hex tokens are rejected".into());
    }
    if range_boundary(Some(b"tx"), 9) != RangeBoundary::Source(b"tx".to_vec())
        || range_boundary(None, 9) != RangeBoundary::Ingest(9)
    {
        return Err("source transaction id is the boundary when present".into());
    }
    let entries = vec![
        CatalogEntry {
            seq: 1,
            boundaries: vec![RangeBoundary::Source(b"A".to_vec())],
        },
        CatalogEntry {
            seq: 2,
            boundaries: vec![RangeBoundary::Source(b"B".to_vec())],
        },
        CatalogEntry {
            seq: 3,
            boundaries: vec![
                RangeBoundary::Source(b"A".to_vec()),
                RangeBoundary::Source(b"B".to_vec()),
            ],
        },
    ];
    if align_start(&entries, 3) != 1 {
        return Err("two tails rewind to the earliest first entry".into());
    }
    if align_start(&entries, 1) != 1 {
        return Err("a start on the first entry stays".into());
    }
    let mut published = Vec::new();
    let first = append_entry(&mut published, vec![RangeBoundary::Ingest(1)]);
    let second = append_entry(&mut published, vec![RangeBoundary::Ingest(2)]);
    if first != 1 || second != 2 || published.len() != 2 {
        return Err("catalog append assigns distinct increasing sequence numbers".into());
    }
    if align_start(&published, 2) != 2 {
        return Err("distinct ingest batch ids do not rewind together".into());
    }
    if global_min(&[20, 10, 15]) != Some(10) || global_min(&[]) != None {
        return Err("global minimum is the lowest cursor, or absent".into());
    }
    if compaction_may_remove(Some(10), 10) || !compaction_may_remove(Some(10), 9) {
        return Err("compaction waits until the minimum is past the segment".into());
    }
    if !compaction_may_remove(None, 99) {
        return Err("no running compute leaves compaction free".into());
    }
    let a = encode_business_key(&[(b"id", b"1")]);
    let b = encode_business_key(&[(b"id", b"2")]);
    if a == b || a == encode_business_key(&[(b"i", b"d1")]) {
        return Err("business-key encoding is one identity".into());
    }
    let tomb = collapse(&high, true, None, false, b"k".to_vec());
    match tomb {
        Collapse::Tombstone { winner, .. } if winner == high => {}
        _ => return Err("delete of a never-stored key is a tombstone".into()),
    }
    let blocked = collapse(&low, false, Some(&high), true, b"k".to_vec());
    match blocked {
        Collapse::Tombstone { winner, .. } if winner == high => {}
        _ => return Err("tombstone incumbent blocks an older upsert".into()),
    }
    let kept = collapse(&high, false, Some(&low), true, b"k".to_vec());
    match kept {
        Collapse::Upsert(winner) if winner == high => {}
        _ => return Err("newer upsert beats a tombstone incumbent".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_covers_the_model() {
        prove_matrix().unwrap();
    }
}
