use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use react_core::keyspace::Keyspace;
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;

#[derive(Clone)]
pub struct RunThreadLogs {
    inner: Arc<Inner>,
}

struct Inner {
    mode: Mode,
    scope: RequestScope,
    tmp_path: PathBuf,
    file: Mutex<File>,
    bound_thread_id: Mutex<Option<String>>,
}

#[derive(Clone)]
enum Mode {
    /// Local filesystem: we can rename the temp file into its final location and keep appending.
    Local { root_dir: PathBuf },
    /// Non-local adapters (e.g. S3): buffer to tmp file and upload at end.
    Buffered,
}

impl RunThreadLogs {
    pub fn new_local(root_dir: impl Into<PathBuf>, scope: RequestScope) -> Result<Self, String> {
        let root_dir: PathBuf = root_dir.into();
        let logs_dir = root_dir
            .join(scope.tenant.trim())
            .join(scope.workspace.trim())
            .join(scope.project_id.trim())
            .join("logs");
        std::fs::create_dir_all(&logs_dir).map_err(|e| e.to_string())?;

        let tmp_path = logs_dir.join(format!(".run_tmp_{}.log", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp_path)
            .map_err(|e| e.to_string())?;

        Ok(Self {
            inner: Arc::new(Inner {
                mode: Mode::Local { root_dir },
                scope,
                tmp_path,
                file: Mutex::new(file),
                bound_thread_id: Mutex::new(None),
            }),
        })
    }

    pub fn new_buffered(scope: RequestScope) -> Result<Self, String> {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push(format!("react-run-{}.log", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp_path)
            .map_err(|e| e.to_string())?;

        Ok(Self {
            inner: Arc::new(Inner {
                mode: Mode::Buffered,
                scope,
                tmp_path,
                file: Mutex::new(file),
                bound_thread_id: Mutex::new(None),
            }),
        })
    }

    pub fn make_writer(&self) -> RunThreadLogWriter {
        RunThreadLogWriter {
            inner: self.inner.clone(),
        }
    }

    pub fn scope(&self) -> RequestScope {
        self.inner.scope.clone()
    }

    pub fn bind_thread_id(
        &self,
        keyspace: &dyn Keyspace,
        thread_id: &str,
    ) -> Result<(), String> {
        // Idempotent.
        {
            let g = self
                .inner
                .bound_thread_id
                .lock()
                .map_err(|_| "thread log lock poisoned".to_string())?;
            if g.as_deref() == Some(thread_id) {
                return Ok(());
            }
        }

        // Record binding first (even if rename/upload comes later).
        {
            let mut g = self
                .inner
                .bound_thread_id
                .lock()
                .map_err(|_| "thread log lock poisoned".to_string())?;
            *g = Some(thread_id.to_string());
        }

        // For local mode, rename temp file into final `{scope}/logs/{thread_id}.log`.
        if let Mode::Local { ref root_dir } = self.inner.mode {
            let key = keyspace.thread_log_key(&self.inner.scope, thread_id)?;
            let final_path = root_dir.join(key);
            if let Some(parent) = final_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Best-effort: if rename fails (e.g. cross-device), fall back to copy+truncate.
            if std::fs::rename(&self.inner.tmp_path, &final_path).is_err() {
                std::fs::copy(&self.inner.tmp_path, &final_path).map_err(|e| e.to_string())?;
                // Keep appending to the original temp file handle; on next finalize we can clean it up.
            }
        }

        Ok(())
    }

    pub async fn upload_if_needed(
        &self,
        storage: Arc<dyn StorageAdapter>,
        keyspace: Arc<dyn Keyspace>,
        thread_id: &str,
    ) -> Result<(), String> {
        // Always bind for consistency (no-op if already bound).
        let _ = self.bind_thread_id(keyspace.as_ref(), thread_id);

        // Local mode: no upload (we write directly to filesystem under root).
        if matches!(self.inner.mode, Mode::Local { .. }) {
            return Ok(());
        }

        // Buffered mode: upload entire temp file at end.
        let key = keyspace.thread_log_key(&self.inner.scope, thread_id)?;
        let path = self.inner.tmp_path.clone();
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path).map_err(|e| e.to_string()))
            .await
            .map_err(|e| e.to_string())??;
        storage.put_bytes(&key, &bytes, "text/plain").await?;

        Ok(())
    }

    pub fn tmp_path(&self) -> PathBuf {
        self.inner.tmp_path.clone()
    }
}

pub struct RunThreadLogWriter {
    inner: Arc<Inner>,
}

impl io::Write for RunThreadLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self
            .inner
            .file
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "thread log lock poisoned"))?;
        g.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut g = self
            .inner
            .file
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "thread log lock poisoned"))?;
        g.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::DefaultKeyspace;

    #[test]
    fn bind_thread_id_local_renames_into_logs_dir() {
        let tmp = std::env::temp_dir().join(format!("react-thread-logs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let logs = RunThreadLogs::new_local(tmp.clone(), scope.clone()).expect("new_local");
        let ks = DefaultKeyspace::new("b".to_string());
        logs.bind_thread_id(&ks, "123").expect("bind");
        let expected = tmp.join("t/w/p/logs/123.log");
        assert!(expected.exists(), "expected log file to exist at {:?}", expected);
        // Best-effort cleanup
        let _ = std::fs::remove_dir_all(tmp);
    }
}

