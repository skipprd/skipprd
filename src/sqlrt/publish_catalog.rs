//! One Skippr catalog sequence for Iceberg and warehouse publishes.
//! Query and serve pin `[align_start, tip]` on this sequence once.

use skippr_cdc::{append_entry, CatalogEntry, RangeBoundary};

use crate::sqlrt::read_boundary::pin_catalog_range;

struct PublishState {
    entries: Vec<CatalogEntry>,
    pin: Option<(u64, u64)>,
}

impl Default for PublishState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            pin: None,
        }
    }
}

#[cfg(not(test))]
fn with_state<R>(f: impl FnOnce(&mut PublishState) -> R) -> R {
    static STATE: std::sync::OnceLock<std::sync::Mutex<PublishState>> = std::sync::OnceLock::new();
    let mut state = STATE
        .get_or_init(|| std::sync::Mutex::new(PublishState::default()))
        .lock()
        .expect("publish catalog");
    f(&mut state)
}

#[cfg(test)]
fn with_state<R>(f: impl FnOnce(&mut PublishState) -> R) -> R {
    thread_local! {
        static STATE: std::cell::RefCell<PublishState> =
            std::cell::RefCell::new(PublishState::default());
    }
    STATE.with(|slot| f(&mut slot.borrow_mut()))
}

/// Iceberg and warehouse both call this. Sequence numbers are distinct and increase.
pub fn publish(boundaries: Vec<RangeBoundary>) -> u64 {
    with_state(|state| append_entry(&mut state.entries, boundaries))
}

/// Pin `[align_start(start), tip]`. A missing catalog keeps the previous pin.
pub fn pin_or_keep(start: u64) -> (u64, u64) {
    with_state(|state| {
        if state.entries.is_empty() {
            return state.pin.unwrap_or((0, u64::MAX));
        }
        let pin = pin_catalog_range(&state.entries, start);
        state.pin = Some(pin);
        pin
    })
}

pub fn entries() -> Vec<CatalogEntry> {
    with_state(|state| state.entries.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_threads_get_distinct_increasing_sequence_numbers() {
        let catalog = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let mut entries = catalog.lock().expect("catalog");
                    append_entry(&mut entries, vec![RangeBoundary::Ingest(1)]);
                });
            }
        });
        let published = catalog.lock().expect("catalog").clone();
        let mut seqs: Vec<_> = published.iter().map(|entry| entry.seq).collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2]);
        assert_eq!(published.len(), 2);
    }

    #[test]
    fn iceberg_and_warehouse_publish_get_distinct_seqs() {
        let iceberg = publish(vec![RangeBoundary::Ingest(1)]);
        let warehouse = publish(vec![RangeBoundary::Ingest(2)]);
        assert_eq!((iceberg, warehouse), (1, 2));
        assert_eq!(entries().len(), 2);
    }

    #[test]
    fn missing_pin_keeps_the_previous_range() {
        publish(vec![RangeBoundary::Ingest(9)]);
        assert_eq!(pin_or_keep(1), (1, 1));
        with_state(|state| state.entries.clear());
        assert_eq!(pin_or_keep(7), (1, 1));
    }

    #[test]
    fn empty_catalog_pin_does_not_filter_wal() {
        assert_eq!(pin_or_keep(1), (0, u64::MAX));
    }
}
