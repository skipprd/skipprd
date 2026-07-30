use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::Notify;

#[derive(Debug)]
struct CatalogOperationBudgetState {
    target: usize,
    active: usize,
}

#[derive(Debug)]
pub struct CatalogOperationBudget {
    state: Mutex<CatalogOperationBudgetState>,
    notify: Notify,
}

impl CatalogOperationBudget {
    pub fn new(target: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CatalogOperationBudgetState {
                target: target.max(1),
                active: 0,
            }),
            notify: Notify::new(),
        })
    }

    pub fn target(&self) -> usize {
        self.state
            .lock()
            .expect("catalog operation budget poisoned")
            .target
    }

    pub fn active(&self) -> usize {
        self.state
            .lock()
            .expect("catalog operation budget poisoned")
            .active
    }

    pub fn set_target(&self, target: usize) {
        let target = target.max(1);
        let grew = {
            let mut state = self
                .state
                .lock()
                .expect("catalog operation budget poisoned");
            let grew = target > state.target;
            state.target = target;
            grew
        };
        if grew {
            self.notify.notify_waiters();
        }
    }

    pub async fn acquire(self: &Arc<Self>) -> CatalogOperationPermit {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .expect("catalog operation budget poisoned");
                if state.active < state.target {
                    state.active += 1;
                    return CatalogOperationPermit {
                        budget: Arc::clone(self),
                    };
                }
            }
            notified.await;
        }
    }

    fn release(&self) {
        {
            let mut state = self
                .state
                .lock()
                .expect("catalog operation budget poisoned");
            state.active = state.active.saturating_sub(1);
        }
        self.notify.notify_one();
    }
}

pub struct CatalogOperationPermit {
    budget: Arc<CatalogOperationBudget>,
}

impl Drop for CatalogOperationPermit {
    fn drop(&mut self) {
        self.budget.release();
    }
}

static PROCESS_CATALOG_OPERATION_BUDGET: Lazy<Arc<CatalogOperationBudget>> = Lazy::new(|| {
    CatalogOperationBudget::new(
        crate::ingest::tuner::current_flush_budget()
            .catalog_operations
            .max(1),
    )
});
static CATALOG_OPERATION_BUDGET_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

pub fn process_catalog_operation_budget() -> Arc<CatalogOperationBudget> {
    Arc::clone(&PROCESS_CATALOG_OPERATION_BUDGET)
}

pub fn refresh_process_catalog_operation_budget() -> usize {
    let target = match CATALOG_OPERATION_BUDGET_OVERRIDE.load(Ordering::Acquire) {
        0 => crate::ingest::tuner::current_flush_budget()
            .catalog_operations
            .max(1),
        target => target,
    };
    PROCESS_CATALOG_OPERATION_BUDGET.set_target(target);
    target
}

#[doc(hidden)]
pub fn set_catalog_operation_budget_override_for_test(target: Option<usize>) {
    CATALOG_OPERATION_BUDGET_OVERRIDE.store(target.unwrap_or(0), Ordering::Release);
    refresh_process_catalog_operation_budget();
}

pub async fn acquire_catalog_operation_permit() -> CatalogOperationPermit {
    PROCESS_CATALOG_OPERATION_BUDGET.acquire().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn growth_and_shrink_only_affect_future_acquisitions() {
        let budget = CatalogOperationBudget::new(2);
        let first = budget.acquire().await;
        let second = budget.acquire().await;
        assert_eq!(budget.active(), 2);

        budget.set_target(1);
        let waiting_budget = Arc::clone(&budget);
        let mut waiting = tokio::spawn(async move { waiting_budget.acquire().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut waiting)
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut waiting)
                .await
                .is_err(),
            "shrink must not admit a waiter while active equals the new target"
        );
        drop(second);
        let third = tokio::time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(budget.active(), 1);

        budget.set_target(3);
        let fourth = budget.acquire().await;
        let fifth = budget.acquire().await;
        assert_eq!(budget.active(), 3);
        drop((third, fourth, fifth));
        assert_eq!(budget.active(), 0);
    }
}
