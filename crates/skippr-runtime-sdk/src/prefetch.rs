//! Byte-bounded prefetch so source plugins can keep fetching while submit waits.
//!
//! The cap is the ready pile ingest threads pull from, not unpersisted WAL bytes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

struct PrefetchState<T> {
    items: VecDeque<(T, usize)>,
    bytes: usize,
    closed: bool,
}

/// Queue that blocks `push` when `queued_bytes + incoming > cap`, except an empty
/// queue always accepts one item (possibly oversized) so a single object cannot stall.
pub struct ByteBoundedPrefetch<T> {
    cap_bytes: usize,
    state: Mutex<PrefetchState<T>>,
    not_empty: Notify,
    not_full: Notify,
}

impl<T: Send> ByteBoundedPrefetch<T> {
    pub fn new(cap_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            cap_bytes: cap_bytes.max(1),
            state: Mutex::new(PrefetchState {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            not_empty: Notify::new(),
            not_full: Notify::new(),
        })
    }

    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    pub fn queued_bytes(&self) -> usize {
        self.state.lock().expect("prefetch lock").bytes
    }

    pub fn close(&self) {
        self.state.lock().expect("prefetch lock").closed = true;
        self.not_empty.notify_waiters();
        self.not_full.notify_waiters();
    }

    /// Close this queue when the guard is dropped so a submit error cannot leave
    /// a producer blocked on `push`.
    pub fn close_on_drop(self: &Arc<Self>) -> PrefetchCloseGuard<T> {
        PrefetchCloseGuard {
            prefetch: Arc::clone(self),
        }
    }

    fn try_push(&self, item: T, bytes: usize) -> Result<Option<(T, usize)>, ()> {
        let mut guard = self.state.lock().expect("prefetch lock");
        if guard.closed {
            return Err(());
        }
        let fits = guard.bytes == 0 || guard.bytes.saturating_add(bytes) <= self.cap_bytes;
        if fits {
            guard.bytes = guard.bytes.saturating_add(bytes);
            guard.items.push_back((item, bytes));
            self.not_empty.notify_one();
            Ok(None)
        } else {
            Ok(Some((item, bytes)))
        }
    }

    pub async fn push(&self, mut item: T, mut bytes: usize) {
        loop {
            let notified = self.not_full.notified();
            match self.try_push(item, bytes) {
                Err(()) => return,
                Ok(None) => return,
                Ok(Some(rejected)) => {
                    item = rejected.0;
                    bytes = rejected.1;
                }
            }
            notified.await;
        }
    }

    pub async fn pop(&self) -> Option<(T, usize)> {
        loop {
            let notified = self.not_empty.notified();
            {
                let mut guard = self.state.lock().expect("prefetch lock");
                if let Some(item) = guard.items.pop_front() {
                    guard.bytes = guard.bytes.saturating_sub(item.1);
                    self.not_full.notify_one();
                    return Some(item);
                }
                if guard.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

/// Closes the prefetch queue on drop.
pub struct PrefetchCloseGuard<T: Send> {
    prefetch: Arc<ByteBoundedPrefetch<T>>,
}

impl<T: Send> Drop for PrefetchCloseGuard<T> {
    fn drop(&mut self) {
        self.prefetch.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn empty_queue_accepts_oversized_item() {
        let q = ByteBoundedPrefetch::new(8);
        q.push("big", 64).await;
        assert_eq!(q.queued_bytes(), 64);
        assert_eq!(q.pop().await, Some(("big", 64)));
        assert_eq!(q.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn push_blocks_when_cap_is_full_until_pop() {
        let q = ByteBoundedPrefetch::new(8);
        q.push("a", 8).await;
        let q_push = Arc::clone(&q);
        let started = Instant::now();
        let push = tokio::spawn(async move {
            q_push.push("b", 1).await;
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!push.is_finished());
        assert_eq!(q.pop().await, Some(("a", 8)));
        push.await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert_eq!(q.pop().await, Some(("b", 1)));
    }

    #[tokio::test]
    async fn close_unblocks_pop_with_none() {
        let q = ByteBoundedPrefetch::<u8>::new(8);
        let q_pop = Arc::clone(&q);
        let pop = tokio::spawn(async move { q_pop.pop().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.close();
        assert_eq!(pop.await.unwrap(), None);
    }

    #[tokio::test]
    async fn close_drops_subsequent_pushes() {
        let q = ByteBoundedPrefetch::new(64);
        q.close();
        q.push("ignored", 4).await;
        assert_eq!(q.pop().await, None);
        assert_eq!(q.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn close_on_drop_unblocks_stuck_push() {
        let q = ByteBoundedPrefetch::new(8);
        q.push("a", 8).await;
        let q_push = Arc::clone(&q);
        let push = tokio::spawn(async move {
            q_push.push("b", 1).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!push.is_finished());
        drop(q.close_on_drop());
        push.await.unwrap();
        assert_eq!(q.pop().await, Some(("a", 8)));
        assert_eq!(q.pop().await, None);
    }
}
