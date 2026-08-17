//! Deterministic crash injection for clustered durable commits.
//!
//! Named points are an enum. The e2e binary arms a point by writing that name
//! to `{pipeline_paths.root}/failpoint`. Crash points consume the file on the
//! first hit (single-shot). [`FailpointName::PreparedDiskFull`] stays armed
//! until the file is removed, so retries keep failing closed. Unknown names
//! fail closed. No cluster env knobs.
//!
//! In-process tests use [`arm`] and get a `DurableError` instead of process abort.

use std::fs;
use std::path::{Path, PathBuf};

use skippr_lease::DurableError;

pub const AFTER_PREPARED: &str = "after_prepared";
pub const REPLICA_AFTER_PREPARED: &str = "replica_after_prepared";
pub const AFTER_REPLICA_ACK: &str = "after_replica_ack";
pub const AFTER_LOCAL_COMMIT: &str = "after_local_commit";
pub const AFTER_OFFSETS_PUBLISHED: &str = "after_offsets_published";
pub const BEFORE_COMPACTION_SINK: &str = "before_compaction_sink";
pub const AFTER_COMPACTION_SINK: &str = "after_compaction_sink";
pub const PREPARED_IO: &str = "prepared_io";
pub const PREPARED_DISK_FULL: &str = "prepared_disk_full";
pub const HOLD_BEFORE_COMPACTION_SINK: &str = "hold_before_compaction_sink";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailpointName {
    AfterPrepared,
    ReplicaAfterPrepared,
    AfterReplicaAck,
    AfterLocalCommit,
    AfterOffsetsPublished,
    BeforeCompactionSink,
    AfterCompactionSink,
    PreparedIo,
    PreparedDiskFull,
    HoldBeforeCompactionSink,
}

impl FailpointName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AfterPrepared => AFTER_PREPARED,
            Self::ReplicaAfterPrepared => REPLICA_AFTER_PREPARED,
            Self::AfterReplicaAck => AFTER_REPLICA_ACK,
            Self::AfterLocalCommit => AFTER_LOCAL_COMMIT,
            Self::AfterOffsetsPublished => AFTER_OFFSETS_PUBLISHED,
            Self::BeforeCompactionSink => BEFORE_COMPACTION_SINK,
            Self::AfterCompactionSink => AFTER_COMPACTION_SINK,
            Self::PreparedIo => PREPARED_IO,
            Self::PreparedDiskFull => PREPARED_DISK_FULL,
            Self::HoldBeforeCompactionSink => HOLD_BEFORE_COMPACTION_SINK,
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DurableError> {
        match raw.trim() {
            AFTER_PREPARED => Ok(Self::AfterPrepared),
            REPLICA_AFTER_PREPARED => Ok(Self::ReplicaAfterPrepared),
            AFTER_REPLICA_ACK => Ok(Self::AfterReplicaAck),
            AFTER_LOCAL_COMMIT => Ok(Self::AfterLocalCommit),
            AFTER_OFFSETS_PUBLISHED => Ok(Self::AfterOffsetsPublished),
            BEFORE_COMPACTION_SINK => Ok(Self::BeforeCompactionSink),
            AFTER_COMPACTION_SINK => Ok(Self::AfterCompactionSink),
            PREPARED_IO => Ok(Self::PreparedIo),
            PREPARED_DISK_FULL => Ok(Self::PreparedDiskFull),
            HOLD_BEFORE_COMPACTION_SINK => Ok(Self::HoldBeforeCompactionSink),
            other => Err(DurableError::ProtocolMismatch(format!(
                "unknown clustered failpoint '{other}'"
            ))),
        }
    }
}

pub fn failpoint_file(dir: &Path) -> std::path::PathBuf {
    dir.join("failpoint")
}

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static ARMED: OnceLock<Mutex<Option<&'static str>>> = OnceLock::new();

#[cfg(test)]
fn slot() -> &'static Mutex<Option<&'static str>> {
    ARMED.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub fn arm(name: &'static str) {
    *slot().lock().expect("failpoint poisoned") = Some(name);
}

#[cfg(not(test))]
pub fn arm(_name: &'static str) {}

#[cfg(test)]
pub fn disarm() {
    *slot().lock().expect("failpoint poisoned") = None;
}

#[cfg(not(test))]
pub fn disarm() {}

#[cfg(test)]
fn take_armed(name: &'static str) -> bool {
    let mut armed = slot().lock().expect("failpoint poisoned");
    if *armed == Some(name) {
        *armed = None;
        true
    } else {
        false
    }
}

#[cfg(not(test))]
fn take_armed(_name: &'static str) -> bool {
    false
}

enum FileHit {
    Miss,
    Fired,
    Invalid(DurableError),
}

fn take_file(dir: &Path, name: &'static str) -> FileHit {
    let path = failpoint_file(dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return FileHit::Miss;
    };
    match FailpointName::parse(&raw) {
        Ok(parsed) if parsed.as_str() == name => {
            let _ = fs::remove_file(&path);
            FileHit::Fired
        }
        Ok(_) => FileHit::Miss,
        Err(err) => {
            let _ = fs::remove_file(&path);
            FileHit::Invalid(err)
        }
    }
}

fn is_hold(name: FailpointName) -> bool {
    matches!(name, FailpointName::HoldBeforeCompactionSink)
}

/// Async wrapper so hold points do not occupy a Tokio worker with `thread::sleep`.
pub async fn hit_async(name: FailpointName, dir: PathBuf) -> Result<(), DurableError> {
    if is_hold(name) {
        return tokio::task::spawn_blocking(move || hit(name, &dir))
            .await
            .map_err(|err| DurableError::Io(err.to_string()))?;
    }
    hit(name, &dir)
}

/// Fire `name` if armed in-process or by `{dir}/failpoint`.
///
/// The e2e binary aborts after consuming the file so the crash is between
/// the named commit-boundary steps. Tests return [`DurableError`] instead of aborting.
/// Hold points wait while the file exists (no abort).
/// [`FailpointName::PreparedDiskFull`] returns [`DurableError::DiskFull`] without aborting.
pub fn hit(name: FailpointName, dir: &Path) -> Result<(), DurableError> {
    let label = name.as_str();
    if is_hold(name) {
        if take_armed(label) {
            return Ok(());
        }
        return wait_hold_file(dir, name);
    }
    if name == FailpointName::PreparedDiskFull {
        if take_armed(label) {
            return Err(DurableError::DiskFull);
        }
        let path = failpoint_file(dir);
        let Ok(raw) = fs::read_to_string(&path) else {
            return Ok(());
        };
        return match FailpointName::parse(&raw) {
            Ok(parsed) if parsed == name => {
                tracing::error!("clustered failpoint disk full");
                Err(DurableError::DiskFull)
            }
            Ok(_) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(&path);
                #[cfg(test)]
                {
                    Err(err)
                }
                #[cfg(not(test))]
                {
                    tracing::error!(error = %err, "invalid clustered failpoint file");
                    std::process::abort();
                }
            }
        };
    }
    if take_armed(label) {
        return Err(DurableError::Io(format!("failpoint {label}")));
    }
    match take_file(dir, label) {
        FileHit::Miss => Ok(()),
        FileHit::Fired => {
            #[cfg(test)]
            {
                Err(DurableError::Io(format!("failpoint {label}")))
            }
            #[cfg(not(test))]
            {
                tracing::error!(name = label, "clustered failpoint abort");
                std::process::abort();
            }
        }
        FileHit::Invalid(err) => {
            #[cfg(test)]
            {
                Err(err)
            }
            #[cfg(not(test))]
            {
                tracing::error!(error = %err, "invalid clustered failpoint file");
                std::process::abort();
            }
        }
    }
}

fn wait_hold_file(dir: &Path, name: FailpointName) -> Result<(), DurableError> {
    let path = failpoint_file(dir);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let mut logged = false;
    loop {
        let Ok(raw) = fs::read_to_string(&path) else {
            return Ok(());
        };
        match FailpointName::parse(&raw) {
            Ok(parsed) if parsed == name => {
                if !logged {
                    tracing::info!("clustered failpoint hold");
                    logged = true;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(DurableError::Io(
                        "clustered failpoint hold timed out".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(_) => return Ok(()),
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("failpoint test lock")
    }

    #[test]
    fn armed_failpoint_fires_once() {
        let _lock = armed_lock();
        let dir = tempfile::tempdir().unwrap();
        arm(AFTER_PREPARED);
        assert!(hit(FailpointName::AfterPrepared, dir.path()).is_err());
        assert!(hit(FailpointName::AfterPrepared, dir.path()).is_ok());
        disarm();
    }

    #[test]
    fn file_failpoint_is_single_shot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(failpoint_file(dir.path()), AFTER_PREPARED).unwrap();
        assert!(hit(FailpointName::AfterPrepared, dir.path()).is_err());
        assert!(!failpoint_file(dir.path()).exists());
        assert!(hit(FailpointName::AfterPrepared, dir.path()).is_ok());
    }

    #[test]
    fn unknown_failpoint_name_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(failpoint_file(dir.path()), "not-a-point").unwrap();
        let err = hit(FailpointName::AfterPrepared, dir.path()).unwrap_err();
        assert!(err.to_string().contains("unknown clustered failpoint"));
        assert!(!failpoint_file(dir.path()).exists());
    }

    #[test]
    fn parse_accepts_only_named_points() {
        assert_eq!(
            FailpointName::parse(AFTER_PREPARED).unwrap(),
            FailpointName::AfterPrepared
        );
        assert_eq!(
            FailpointName::parse(AFTER_OFFSETS_PUBLISHED).unwrap(),
            FailpointName::AfterOffsetsPublished
        );
        assert_eq!(
            FailpointName::parse(HOLD_BEFORE_COMPACTION_SINK).unwrap(),
            FailpointName::HoldBeforeCompactionSink
        );
        assert_eq!(
            FailpointName::parse(PREPARED_DISK_FULL).unwrap(),
            FailpointName::PreparedDiskFull
        );
        assert!(FailpointName::parse("lease_timeout").is_err());
    }

    #[test]
    fn prepared_disk_full_returns_disk_full() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(failpoint_file(dir.path()), PREPARED_DISK_FULL).unwrap();
        let err = hit(FailpointName::PreparedDiskFull, dir.path()).unwrap_err();
        assert!(matches!(err, DurableError::DiskFull));
        assert!(failpoint_file(dir.path()).exists());
        assert!(matches!(
            hit(FailpointName::PreparedDiskFull, dir.path()).unwrap_err(),
            DurableError::DiskFull
        ));
        fs::remove_file(failpoint_file(dir.path())).unwrap();
        assert!(hit(FailpointName::PreparedDiskFull, dir.path()).is_ok());
    }

    #[test]
    fn every_named_point_parses_and_fires_once() {
        let _lock = armed_lock();
        let names = [
            FailpointName::AfterPrepared,
            FailpointName::ReplicaAfterPrepared,
            FailpointName::AfterReplicaAck,
            FailpointName::AfterLocalCommit,
            FailpointName::AfterOffsetsPublished,
            FailpointName::BeforeCompactionSink,
            FailpointName::AfterCompactionSink,
            FailpointName::PreparedIo,
        ];
        for name in names {
            let dir = tempfile::tempdir().unwrap();
            arm(name.as_str());
            assert!(hit(name, dir.path()).is_err(), "{}", name.as_str());
            assert!(hit(name, dir.path()).is_ok(), "{}", name.as_str());
            disarm();
        }
    }

    #[test]
    fn hold_before_compaction_sink_waits_until_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = failpoint_file(dir.path());
        fs::write(&path, HOLD_BEFORE_COMPACTION_SINK).unwrap();
        let dir_path = dir.path().to_path_buf();
        let worker =
            std::thread::spawn(move || hit(FailpointName::HoldBeforeCompactionSink, &dir_path));
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(
            FailpointName::parse(HOLD_BEFORE_COMPACTION_SINK).unwrap(),
            FailpointName::HoldBeforeCompactionSink
        );
    }

    #[tokio::test]
    async fn hit_async_hold_does_not_block_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = failpoint_file(dir.path());
        fs::write(&path, HOLD_BEFORE_COMPACTION_SINK).unwrap();
        let dir_path = dir.path().to_path_buf();
        let hold = tokio::spawn(hit_async(FailpointName::HoldBeforeCompactionSink, dir_path));
        let ticker = tokio::spawn(async {
            let mut ticks = 0u32;
            while ticks < 8 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                ticks += 1;
            }
            ticks
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        fs::remove_file(&path).unwrap();
        hold.await.unwrap().unwrap();
        assert!(ticker.await.unwrap() >= 8);
    }

    #[test]
    fn hold_waits_only_for_matching_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(failpoint_file(dir.path()), HOLD_BEFORE_COMPACTION_SINK).unwrap();
        hit(FailpointName::AfterPrepared, dir.path()).unwrap();
        assert!(failpoint_file(dir.path()).exists());
    }
}
