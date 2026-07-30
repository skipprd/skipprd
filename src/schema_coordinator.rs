use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, Notify};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SchemaPublicationKey {
    scope: String,
    namespace: String,
    version: u64,
}

#[derive(Default)]
struct SchemaCoordinatorState {
    flights: HashMap<SchemaPublicationKey, Arc<SchemaFlight>>,
    namespace_locks: HashMap<(String, String), Arc<AsyncMutex<()>>>,
    published_versions: HashMap<(String, String), u64>,
}

#[derive(Default)]
struct SchemaCoordinatorInner {
    state: Mutex<SchemaCoordinatorState>,
}

#[derive(Default)]
struct SchemaFlight {
    result: Mutex<Option<Result<(), String>>>,
    notify: Notify,
    waiters: AtomicUsize,
}

struct SchemaFlightWaiter<'a> {
    waiters: &'a AtomicUsize,
}

impl Drop for SchemaFlightWaiter<'_> {
    fn drop(&mut self) {
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SchemaFlight {
    async fn wait(&self) -> Result<(), String> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        let _waiter = SchemaFlightWaiter {
            waiters: &self.waiters,
        };
        loop {
            let notified = self.notify.notified();
            if let Some(result) = self
                .result
                .lock()
                .expect("schema flight result lock poisoned")
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

struct SchemaFlightLeader {
    inner: Arc<SchemaCoordinatorInner>,
    key: SchemaPublicationKey,
    flight: Arc<SchemaFlight>,
    completed: bool,
}

impl SchemaFlightLeader {
    fn complete(&mut self, result: Result<(), String>) {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("schema coordinator state lock poisoned");
            if result.is_ok() {
                state
                    .published_versions
                    .entry((self.key.scope.clone(), self.key.namespace.clone()))
                    .and_modify(|version| *version = (*version).max(self.key.version))
                    .or_insert(self.key.version);
            }
            if state
                .flights
                .get(&self.key)
                .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
            {
                state.flights.remove(&self.key);
            }
        }
        *self
            .flight
            .result
            .lock()
            .expect("schema flight result lock poisoned") = Some(result);
        self.completed = true;
        self.flight.notify.notify_waiters();
    }
}

impl Drop for SchemaFlightLeader {
    fn drop(&mut self) {
        if !self.completed {
            self.complete(Err(format!(
                "schema publication for scope '{}' namespace '{}' version {} was cancelled",
                self.key.scope, self.key.namespace, self.key.version
            )));
        }
    }
}

/// Deduplicates schema publication by namespace and version.
///
/// Exact concurrent requests share one result. Successful versions remain
/// satisfied, failures wake all waiters and are removed so a later caller can
/// retry. A per-namespace lock preserves schema ordering without coupling
/// unrelated namespaces.
#[derive(Clone, Default)]
pub(crate) struct SchemaCoordinator {
    inner: Arc<SchemaCoordinatorInner>,
}

impl SchemaCoordinator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn coordinate<F, Fut>(
        &self,
        scope: &str,
        namespace: &str,
        version: u64,
        operation: F,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        let key = SchemaPublicationKey {
            scope: scope.to_string(),
            namespace: namespace.to_string(),
            version,
        };
        let scope_namespace = (scope.to_string(), namespace.to_string());
        let (flight, namespace_lock, is_leader) = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("schema coordinator state lock poisoned");
            if state
                .published_versions
                .get(&scope_namespace)
                .is_some_and(|published| *published >= version)
            {
                return Ok(());
            }
            if let Some(flight) = state.flights.get(&key) {
                (Arc::clone(flight), None, false)
            } else {
                let flight = Arc::new(SchemaFlight::default());
                let namespace_lock = state
                    .namespace_locks
                    .entry(scope_namespace.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                    .clone();
                state.flights.insert(key.clone(), Arc::clone(&flight));
                (flight, Some(namespace_lock), true)
            }
        };

        if !is_leader {
            return flight.wait().await;
        }

        let mut leader = SchemaFlightLeader {
            inner: Arc::clone(&self.inner),
            key,
            flight,
            completed: false,
        };
        let namespace_lock = namespace_lock.expect("schema flight leader must own namespace lock");
        let _namespace_guard = namespace_lock.lock().await;
        let already_published = self
            .inner
            .state
            .lock()
            .expect("schema coordinator state lock poisoned")
            .published_versions
            .get(&scope_namespace)
            .is_some_and(|published| *published >= version);
        let result = if already_published {
            Ok(())
        } else {
            operation().await
        };
        leader.complete(result.clone());
        result
    }

    #[cfg(test)]
    fn waiter_count(&self, scope: &str, namespace: &str, version: u64) -> usize {
        self.inner
            .state
            .lock()
            .expect("schema coordinator state lock poisoned")
            .flights
            .get(&SchemaPublicationKey {
                scope: scope.to_string(),
                namespace: namespace.to_string(),
                version,
            })
            .map(|flight| flight.waiters.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::{oneshot, Semaphore};

    use super::SchemaCoordinator;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_async_and_blocking_callers_share_one_rpc() {
        let coordinator = SchemaCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let (started_tx, started_rx) = oneshot::channel();

        let async_coordinator = coordinator.clone();
        let async_calls = Arc::clone(&calls);
        let async_release = Arc::clone(&release);
        let async_call = tokio::spawn(async move {
            async_coordinator
                .coordinate("test", "events", 7, || async move {
                    async_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    async_release.acquire().await.unwrap().forget();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();

        let blocking_coordinator = coordinator.clone();
        let blocking_calls = Arc::clone(&calls);
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(
                    blocking_coordinator.coordinate("test", "events", 7, || async move {
                        blocking_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }),
                )
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.waiter_count("test", "events", 7) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking caller should join the in-flight publication");
        release.add_permits(1);
        assert_eq!(async_call.await.unwrap(), Ok(()));
        assert_eq!(
            tokio::task::spawn_blocking(move || thread.join().unwrap())
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failure_wakes_waiters_and_remains_retryable() {
        let coordinator = SchemaCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let (started_tx, started_rx) = oneshot::channel();

        let first_coordinator = coordinator.clone();
        let first_calls = Arc::clone(&calls);
        let first_release = Arc::clone(&release);
        let first = tokio::spawn(async move {
            first_coordinator
                .coordinate("test", "events", 9, || async move {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    first_release.acquire().await.unwrap().forget();
                    Err("simulated schema failure".to_string())
                })
                .await
        });
        started_rx.await.unwrap();

        let second_coordinator = coordinator.clone();
        let second_calls = Arc::clone(&calls);
        let second = tokio::spawn(async move {
            second_coordinator
                .coordinate("test", "events", 9, || async move {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.waiter_count("test", "events", 9) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second caller should await the failed publication");
        release.add_permits(1);
        assert_eq!(
            first.await.unwrap(),
            Err("simulated schema failure".to_string())
        );
        assert_eq!(
            second.await.unwrap(),
            Err("simulated schema failure".to_string())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        coordinator
            .coordinate("test", "events", 9, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn blocked_namespace_does_not_hold_unrelated_namespace() {
        let coordinator = SchemaCoordinator::new();
        let release = Arc::new(Semaphore::new(0));
        let (started_tx, started_rx) = oneshot::channel();

        let blocked_coordinator = coordinator.clone();
        let blocked_release = Arc::clone(&release);
        let blocked = tokio::spawn(async move {
            blocked_coordinator
                .coordinate("test", "blocked", 3, || async move {
                    let _ = started_tx.send(());
                    blocked_release.acquire().await.unwrap().forget();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            coordinator.coordinate("test", "unrelated", 3, || async { Ok(()) }),
        )
        .await
        .expect("unrelated namespace should not wait")
        .unwrap();

        release.add_permits(1);
        blocked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn identical_namespace_versions_are_isolated_by_scope() {
        let coordinator = SchemaCoordinator::new();
        let calls = AtomicUsize::new(0);

        coordinator
            .coordinate("pipeline-a", "events", 3, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        coordinator
            .coordinate("pipeline-b", "events", 3, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        coordinator
            .coordinate("pipeline-a", "events", 3, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
