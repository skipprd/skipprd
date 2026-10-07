use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use iceberg::io::FileIO;
use iceberg::spec::{TableMetadata, TableMetadataRef};
use iceberg::Result;

/// Iceberg metadata files are write-once, so an entry keyed by its location
/// never goes stale. The bound only caps memory.
const METADATA_CACHE_ENTRIES: usize = 64;

/// Parsed metadata plus the canonical location it was written to. Aliases
/// (Hadoop `v{N}.metadata.json`) map to the canonical iceberg-rust location.
#[derive(Clone, Debug)]
pub struct CachedMetadata {
    pub location: String,
    pub metadata: TableMetadataRef,
}

#[derive(Debug, Default)]
struct Entries {
    map: HashMap<String, CachedMetadata>,
    order: VecDeque<String>,
}

/// Bounded, shared cache of parsed table metadata keyed by metadata location.
#[derive(Clone, Debug, Default)]
pub struct MetadataCache {
    entries: Arc<Mutex<Entries>>,
}

impl MetadataCache {
    pub fn get(&self, key: &str) -> Option<CachedMetadata> {
        self.lock().map.get(key).cloned()
    }

    pub fn insert(&self, key: impl Into<String>, entry: CachedMetadata) {
        let key = key.into();
        let mut entries = self.lock();
        if entries.map.insert(key.clone(), entry).is_some() {
            return;
        }
        entries.order.push_back(key);
        while entries.order.len() > METADATA_CACHE_ENTRIES {
            if let Some(evicted) = entries.order.pop_front() {
                entries.map.remove(&evicted);
            }
        }
    }

    /// Forget alias keys whose target can be rewritten (a dropped table's
    /// Hadoop aliases). Canonical uuid locations never need this.
    pub fn remove_prefix(&self, prefix: &str) {
        let mut entries = self.lock();
        entries.map.retain(|key, _| !key.starts_with(prefix));
        let Entries { map, order } = &mut *entries;
        order.retain(|key| map.contains_key(key));
    }

    /// Record metadata the caller just wrote so the next load skips the read.
    pub fn insert_written(&self, location: &str, metadata: TableMetadataRef) {
        self.insert(
            location,
            CachedMetadata {
                location: location.to_string(),
                metadata,
            },
        );
    }

    pub async fn read(&self, file_io: &FileIO, location: &str) -> Result<TableMetadataRef> {
        if let Some(hit) = self.get(location) {
            return Ok(hit.metadata);
        }
        let metadata = Arc::new(TableMetadata::read_from(file_io, location).await?);
        self.insert_written(location, Arc::clone(&metadata));
        Ok(metadata)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

const DELETE_AFTER_COMMIT: &str = "write.metadata.delete-after-commit.enabled";

/// Metadata files a commit dropped from `metadata-log`, when the table sets
/// `write.metadata.delete-after-commit.enabled`.
fn superseded_metadata_locations(
    before: &TableMetadata,
    before_location: &str,
    after: &TableMetadata,
    after_location: &str,
) -> Vec<String> {
    if after
        .properties()
        .get(DELETE_AFTER_COMMIT)
        .map(String::as_str)
        != Some("true")
    {
        return Vec::new();
    }
    let kept: std::collections::HashSet<&str> = after
        .metadata_log()
        .iter()
        .map(|entry| entry.metadata_file.as_str())
        .chain(std::iter::once(after_location))
        .collect();
    before
        .metadata_log()
        .iter()
        .map(|entry| entry.metadata_file.as_str())
        .chain(std::iter::once(before_location))
        .filter(|location| !kept.contains(location))
        .map(str::to_string)
        .collect()
}

/// Best-effort delete after the pointer swap committed: a leftover file is an
/// orphan, never a correctness problem, so failures are not returned.
pub async fn delete_superseded_metadata(
    file_io: &FileIO,
    before: &TableMetadata,
    before_location: &str,
    after: &TableMetadata,
    after_location: &str,
) -> Vec<String> {
    let mut deleted = Vec::new();
    for location in superseded_metadata_locations(before, before_location, after, after_location) {
        if file_io.delete(&location).await.is_ok() {
            deleted.push(location);
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg::spec::{NestedField, PrimitiveType, Schema, TableMetadataBuilder, Type};
    use iceberg::TableCreation;

    fn metadata(location: &str) -> TableMetadata {
        let schema = Schema::builder()
            .with_fields(vec![NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )
            .into()])
            .build()
            .unwrap();
        TableMetadataBuilder::from_table_creation(
            TableCreation::builder()
                .name("orders".into())
                .location(location.to_string())
                .schema(schema)
                .build(),
        )
        .unwrap()
        .build()
        .unwrap()
        .metadata
    }

    #[tokio::test]
    async fn second_read_of_a_location_does_not_touch_storage() {
        let dir = tempfile::tempdir().unwrap();
        let table = format!("file://{}/orders", dir.path().display());
        let location = format!("{table}/metadata/00000-a.metadata.json");
        let file_io = FileIO::new_with_fs();
        metadata(&table)
            .write_to(&file_io, &location)
            .await
            .unwrap();

        let cache = MetadataCache::default();
        let first = cache.read(&file_io, &location).await.unwrap();
        file_io.delete(&location).await.unwrap();
        let second = cache.read(&file_io, &location).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn capacity_evicts_the_oldest_location_first() {
        let cache = MetadataCache::default();
        let shared = Arc::new(metadata("file:///tmp/orders"));
        for i in 0..=METADATA_CACHE_ENTRIES {
            cache.insert_written(&format!("loc-{i}"), Arc::clone(&shared));
        }
        assert!(cache.get("loc-0").is_none());
        assert!(cache.get("loc-1").is_some());
        assert!(cache
            .get(&format!("loc-{METADATA_CACHE_ENTRIES}"))
            .is_some());
        assert_eq!(cache.lock().map.len(), METADATA_CACHE_ENTRIES);
    }

    #[test]
    fn alias_keys_resolve_to_the_canonical_location() {
        let cache = MetadataCache::default();
        let shared = Arc::new(metadata("file:///tmp/orders"));
        cache.insert(
            "file:///tmp/orders/metadata/v3.metadata.json",
            CachedMetadata {
                location: "file:///tmp/orders/metadata/00003-b.metadata.json".into(),
                metadata: shared,
            },
        );
        let hit = cache
            .get("file:///tmp/orders/metadata/v3.metadata.json")
            .unwrap();
        assert!(hit.location.ends_with("00003-b.metadata.json"));
    }
}
