use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Monotonic nanoseconds from a [`Clock`] origin. Tests can construct and advance this.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MonoInstant(u64);

impl MonoInstant {
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    pub fn saturating_add(self, duration: std::time::Duration) -> Self {
        Self(self.0.saturating_add(duration.as_nanos() as u64))
    }
}

impl std::ops::Add<std::time::Duration> for MonoInstant {
    type Output = Self;

    fn add(self, rhs: std::time::Duration) -> Self::Output {
        self.saturating_add(rhs)
    }
}

pub trait Clock: Send + Sync {
    fn monotonic_now(&self) -> MonoInstant;
    fn unix_millis(&self) -> u64;
}

#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep_until(&self, deadline: MonoInstant);
}

/// Race `fut` against a sleeper-driven deadline. Tests drive this with [`TestClock`].
pub async fn race_deadline<T>(
    sleeper: &dyn Sleeper,
    clock: &dyn Clock,
    duration: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        _ = sleeper.sleep_until(clock.monotonic_now().saturating_add(duration)) => Err(()),
        result = fut => Ok(result),
    }
}

pub type AsyncSleeper = dyn Sleeper;

pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_now(&self) -> MonoInstant {
        MonoInstant(self.origin.elapsed().as_nanos() as u64)
    }

    fn unix_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

pub struct TestClock {
    nanos: AtomicU64,
    unix_millis: AtomicU64,
    notify: Notify,
}

impl TestClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            nanos: AtomicU64::new(0),
            unix_millis: AtomicU64::new(0),
            notify: Notify::new(),
        })
    }

    pub fn advance(&self, duration: std::time::Duration) {
        self.nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn set_unix_millis(&self, millis: u64) {
        self.unix_millis.store(millis, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn monotonic_now(&self) -> MonoInstant {
        MonoInstant(self.nanos.load(Ordering::SeqCst))
    }

    fn unix_millis(&self) -> u64 {
        self.unix_millis.load(Ordering::SeqCst)
    }
}

pub struct TestSleeper {
    clock: Arc<TestClock>,
}

impl TestSleeper {
    pub fn new(clock: Arc<TestClock>) -> Self {
        Self { clock }
    }
}

#[async_trait]
impl Sleeper for TestSleeper {
    async fn sleep_until(&self, deadline: MonoInstant) {
        loop {
            let now = self.clock.monotonic_now();
            if now >= deadline {
                return;
            }
            let remaining = deadline.as_nanos().saturating_sub(now.as_nanos());
            self.clock
                .advance(std::time::Duration::from_nanos(remaining));
            tokio::task::yield_now().await;
        }
    }
}

pub struct TokioSleeper {
    clock: Arc<dyn Clock>,
}

impl TokioSleeper {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }
}

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep_until(&self, deadline: MonoInstant) {
        loop {
            let now = self.clock.monotonic_now();
            if now >= deadline {
                return;
            }
            let remaining_ns = deadline.as_nanos().saturating_sub(now.as_nanos());
            tokio::time::sleep(std::time::Duration::from_nanos(remaining_ns)).await;
        }
    }
}
