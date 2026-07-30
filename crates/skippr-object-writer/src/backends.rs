//! Reusable atomic backends for object-store and local-file sinks.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use object_store::path::Path as ObjectPath;
use object_store::{
    Attribute, Attributes, MultipartUpload as ObjectStoreMultipartUpload, ObjectStore,
    PutMultipartOptions,
};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};

use crate::{
    CompletionMetadata, MultipartUpload, ObjectPartReceipt, ObjectWriteBackend, ObjectWriteRequest,
    PartMetadata,
};

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("object store operation failed: {0}")]
    ObjectStore(#[from] object_store::Error),
    #[error("file operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("unknown object upload {0}")]
    UnknownUpload(String),
    #[error("upload {upload_id} received part {actual}, expected {expected}")]
    UnexpectedPart {
        upload_id: String,
        actual: u32,
        expected: u32,
    },
}

struct ObjectStoreUploadState {
    upload: Box<dyn ObjectStoreMultipartUpload>,
    next_part_number: u32,
}

struct ObjectStoreUpload {
    state: Mutex<ObjectStoreUploadState>,
    part_ready: Notify,
}

/// Multipart/resumable adapter for `object_store` implementations.
///
/// GCS and Azure Blob both provide atomic multipart completion through this
/// API. Part futures may complete concurrently while invocation order remains
/// deterministic.
pub struct ObjectStoreBackend {
    store: Arc<dyn ObjectStore>,
    uploads: Mutex<HashMap<String, Arc<ObjectStoreUpload>>>,
    next_upload_id: AtomicU64,
}

impl ObjectStoreBackend {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            uploads: Mutex::new(HashMap::new()),
            next_upload_id: AtomicU64::new(1),
        }
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    async fn upload(&self, upload_id: &str) -> Result<Arc<ObjectStoreUpload>, BackendError> {
        self.uploads
            .lock()
            .await
            .get(upload_id)
            .cloned()
            .ok_or_else(|| BackendError::UnknownUpload(upload_id.to_string()))
    }
}

#[async_trait]
impl ObjectWriteBackend for ObjectStoreBackend {
    type Error = BackendError;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        let path = ObjectPath::from(request.object_key.clone());
        let mut attributes = Attributes::with_capacity(request.metadata.len() + 1);
        attributes.insert(Attribute::ContentType, request.content_type.clone().into());
        for (key, value) in &request.metadata {
            attributes.insert(
                Attribute::Metadata(key.clone().into()),
                value.clone().into(),
            );
        }
        let upload = self
            .store
            .put_multipart_opts(
                &path,
                PutMultipartOptions {
                    attributes,
                    ..Default::default()
                },
            )
            .await?;
        let upload_id = format!(
            "object-store-{}",
            self.next_upload_id.fetch_add(1, Ordering::Relaxed)
        );
        self.uploads.lock().await.insert(
            upload_id.clone(),
            Arc::new(ObjectStoreUpload {
                state: Mutex::new(ObjectStoreUploadState {
                    upload,
                    next_part_number: 1,
                }),
                part_ready: Notify::new(),
            }),
        );
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::new(),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let upload_state = self.upload(&upload.upload_id).await?;
        loop {
            let notified = upload_state.part_ready.notified();
            let mut state = upload_state.state.lock().await;
            if part_number < state.next_part_number {
                return Err(BackendError::UnexpectedPart {
                    upload_id: upload.upload_id.clone(),
                    actual: part_number,
                    expected: state.next_part_number,
                });
            }
            if part_number == state.next_part_number {
                let part = state.upload.put_part(bytes.into());
                state.next_part_number =
                    state
                        .next_part_number
                        .checked_add(1)
                        .ok_or(BackendError::UnexpectedPart {
                            upload_id: upload.upload_id.clone(),
                            actual: part_number,
                            expected: state.next_part_number,
                        })?;
                drop(state);
                upload_state.part_ready.notify_waiters();
                part.await?;
                return Ok(PartMetadata::default());
            }
            drop(state);
            notified.await;
        }
    }

    async fn complete(
        &self,
        upload: &MultipartUpload,
        _parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error> {
        let upload_state = self.upload(&upload.upload_id).await?;
        let result = upload_state.state.lock().await.upload.complete().await?;
        self.uploads.lock().await.remove(&upload.upload_id);
        Ok(CompletionMetadata {
            etag: result.e_tag,
            version_id: result.version,
            ..Default::default()
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        let Some(upload_state) = self.uploads.lock().await.remove(&upload.upload_id) else {
            return Ok(());
        };
        upload_state.state.lock().await.upload.abort().await?;
        Ok(())
    }
}

struct FileUploadState {
    file: Option<File>,
    next_part_number: u32,
}

struct FileUpload {
    final_path: PathBuf,
    temporary_path: PathBuf,
    state: Mutex<FileUploadState>,
    part_ready: Notify,
}

/// Local filesystem adapter that writes a sibling temporary file and atomically
/// renames it over the deterministic final target only after Parquet closes.
pub struct AtomicFileBackend {
    uploads: Mutex<HashMap<String, Arc<FileUpload>>>,
    next_upload_id: AtomicU64,
}

impl Default for AtomicFileBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicFileBackend {
    pub fn new() -> Self {
        Self {
            uploads: Mutex::new(HashMap::new()),
            next_upload_id: AtomicU64::new(1),
        }
    }

    async fn upload(&self, upload_id: &str) -> Result<Arc<FileUpload>, BackendError> {
        self.uploads
            .lock()
            .await
            .get(upload_id)
            .cloned()
            .ok_or_else(|| BackendError::UnknownUpload(upload_id.to_string()))
    }
}

#[async_trait]
impl ObjectWriteBackend for AtomicFileBackend {
    type Error = BackendError;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        let final_path = PathBuf::from(&request.object_key);
        let parent = final_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "object path has no parent")
        })?;
        tokio::fs::create_dir_all(parent).await?;

        let sequence = self.next_upload_id.fetch_add(1, Ordering::Relaxed);
        let upload_id = format!("file-{sequence}");
        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid object filename")
            })?;
        let temporary_path = parent.join(format!(".{file_name}.skippr-{sequence}.tmp"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await?;
        self.uploads.lock().await.insert(
            upload_id.clone(),
            Arc::new(FileUpload {
                final_path,
                temporary_path: temporary_path.clone(),
                state: Mutex::new(FileUploadState {
                    file: Some(file),
                    next_part_number: 1,
                }),
                part_ready: Notify::new(),
            }),
        );
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::from([(
                "temporary_path".to_string(),
                temporary_path.to_string_lossy().into_owned(),
            )]),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let upload_state = self.upload(&upload.upload_id).await?;
        loop {
            let notified = upload_state.part_ready.notified();
            let mut state = upload_state.state.lock().await;
            if part_number < state.next_part_number {
                return Err(BackendError::UnexpectedPart {
                    upload_id: upload.upload_id.clone(),
                    actual: part_number,
                    expected: state.next_part_number,
                });
            }
            if part_number == state.next_part_number {
                state
                    .file
                    .as_mut()
                    .ok_or_else(|| BackendError::UnknownUpload(upload.upload_id.clone()))?
                    .write_all(&bytes)
                    .await?;
                state.next_part_number =
                    state
                        .next_part_number
                        .checked_add(1)
                        .ok_or(BackendError::UnexpectedPart {
                            upload_id: upload.upload_id.clone(),
                            actual: part_number,
                            expected: state.next_part_number,
                        })?;
                drop(state);
                upload_state.part_ready.notify_waiters();
                return Ok(PartMetadata::default());
            }
            drop(state);
            notified.await;
        }
    }

    async fn complete(
        &self,
        upload: &MultipartUpload,
        parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error> {
        let upload_state = self.upload(&upload.upload_id).await?;
        {
            let mut state = upload_state.state.lock().await;
            let file = state
                .file
                .as_mut()
                .ok_or_else(|| BackendError::UnknownUpload(upload.upload_id.clone()))?;
            file.flush().await?;
            file.sync_all().await?;
            state.file.take();
        }
        tokio::fs::rename(&upload_state.temporary_path, &upload_state.final_path).await?;
        self.uploads.lock().await.remove(&upload.upload_id);
        let bytes = parts.iter().map(|part| part.bytes).sum::<u64>();
        Ok(CompletionMetadata {
            etag: Some(format!("local-{bytes}")),
            metadata: BTreeMap::from([(
                "final_path".to_string(),
                upload_state.final_path.to_string_lossy().into_owned(),
            )]),
            ..Default::default()
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        let Some(upload_state) = self.uploads.lock().await.remove(&upload.upload_id) else {
            return Ok(());
        };
        upload_state.state.lock().await.file.take();
        match tokio::fs::remove_file(&upload_state.temporary_path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use futures::{stream, TryStreamExt};
    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;
    use parquet::file::properties::WriterProperties;

    use super::*;
    use crate::{ObjectWriteSession, ObjectWriterConfig};

    fn request(key: impl Into<String>) -> ObjectWriteRequest {
        ObjectWriteRequest::new(key)
    }

    #[tokio::test]
    async fn object_store_backend_commits_parts_in_number_order() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let backend = ObjectStoreBackend::new(store.clone());
        let upload = backend
            .begin(&request("root/object.parquet"))
            .await
            .unwrap();

        let (second, first) = tokio::join!(
            backend.upload_part(&upload, 2, Bytes::from_static(b"second")),
            backend.upload_part(&upload, 1, Bytes::from_static(b"first-"))
        );
        first.unwrap();
        second.unwrap();
        backend.complete(&upload, &[]).await.unwrap();

        let bytes = store
            .get(&ObjectPath::from("root/object.parquet"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"first-second"));
    }

    #[tokio::test]
    async fn object_store_backend_abort_removes_uncommitted_upload() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let backend = ObjectStoreBackend::new(store.clone());
        let upload = backend
            .begin(&request("root/aborted.parquet"))
            .await
            .unwrap();
        backend
            .upload_part(&upload, 1, Bytes::from_static(b"partial"))
            .await
            .unwrap();
        backend.abort(&upload).await.unwrap();

        assert!(store
            .get(&ObjectPath::from("root/aborted.parquet"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn streamed_batches_commit_as_one_logical_object() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let backend = Arc::new(ObjectStoreBackend::new(store.clone()));
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batches = vec![
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1, 2]))])
                .unwrap(),
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![3, 4]))])
                .unwrap(),
        ];
        let receipt = ObjectWriteSession::new(
            backend,
            request("root/group.parquet"),
            ObjectWriterConfig {
                part_size: 128,
                batch_channel_capacity: 1,
                byte_channel_capacity: 1,
                max_in_flight_parts: 2,
            },
        )
        .unwrap()
        .write_parquet(
            schema,
            WriterProperties::builder().build(),
            stream::iter(batches.into_iter().map(Ok)),
        )
        .await
        .unwrap();

        let objects = store.list(None).try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].location.as_ref(), "root/group.parquet");
        assert_eq!(receipt.rows, 4);
        assert!(receipt.transport_chunk_count > 1);
    }

    #[tokio::test]
    async fn atomic_file_backend_renames_only_on_complete_and_cleans_abort() {
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("nested/object.parquet");
        let backend = AtomicFileBackend::new();
        let upload = backend
            .begin(&request(final_path.to_string_lossy()))
            .await
            .unwrap();
        let temporary_path = PathBuf::from(upload.metadata.get("temporary_path").unwrap());
        backend
            .upload_part(&upload, 1, Bytes::from_static(b"parquet"))
            .await
            .unwrap();
        assert!(!final_path.exists());
        assert!(temporary_path.exists());

        backend.complete(&upload, &[]).await.unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"parquet");
        assert!(!temporary_path.exists());

        let aborted = backend
            .begin(&request(final_path.to_string_lossy()))
            .await
            .unwrap();
        let aborted_path = PathBuf::from(aborted.metadata.get("temporary_path").unwrap());
        backend.abort(&aborted).await.unwrap();
        assert!(!aborted_path.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"parquet");
    }

    #[test]
    fn migrated_hot_paths_cannot_restore_full_parquet_buffers() {
        let sources = [
            include_str!("../../../plugins/data_sink/s3/src/s3.rs"),
            include_str!("../../../plugins/data_sink/gcs/src/gcs.rs"),
            include_str!("../../../plugins/data_sink/azure-blob/src/azure_blob.rs"),
            include_str!("../../../plugins/data_sink/file/src/main.rs"),
            include_str!("../../../plugins/data_sink/sftp/src/sftp.rs"),
        ];
        for source in sources {
            assert!(
                !source.contains("serialize_to_parquet("),
                "migrated object sink must use ObjectWriteSession"
            );
            assert!(
                source.contains("reader.into_stream()"),
                "grouped sink must stream one logical object"
            );
        }
    }
}
