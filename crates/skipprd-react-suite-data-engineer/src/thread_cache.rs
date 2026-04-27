use dashmap::DashMap;
use once_cell::sync::OnceCell;
use std::time::Instant;

const MAX_THREAD_CACHE_ENTRIES: usize = 1000;

#[derive(Clone, Debug, Default)]
pub struct ThreadCache {
    pub published_relations: Vec<String>,
    pub published_manifest_sha256: Option<String>,
    pub updated_at: Option<Instant>,
}

static THREAD_CTX_CACHE: OnceCell<DashMap<String, ThreadCache>> = OnceCell::new();
fn ctx_cache() -> &'static DashMap<String, ThreadCache> {
    THREAD_CTX_CACHE.get_or_init(DashMap::new)
}

fn evict_if_full(cache: &DashMap<String, ThreadCache>) {
    if cache.len() > MAX_THREAD_CACHE_ENTRIES {
        tracing::warn!(
            entries = cache.len(),
            limit = MAX_THREAD_CACHE_ENTRIES,
            "thread cache exceeded limit; clearing"
        );
        cache.clear();
    }
}

pub struct ThreadCacheStore;

impl ThreadCacheStore {
    pub fn get(thread_id: &str) -> Option<ThreadCache> {
        ctx_cache().get(thread_id).map(|c| c.clone())
    }

    pub fn update_published(thread_id: &str, manifest_sha256: &str, relations: Vec<String>) {
        let cache = ctx_cache();
        evict_if_full(cache);
        let mut entry = cache.get(thread_id).map(|e| e.clone()).unwrap_or_default();
        entry.published_relations = relations;
        entry.published_manifest_sha256 = Some(manifest_sha256.to_string());
        entry.updated_at = Some(Instant::now());
        cache.insert(thread_id.to_string(), entry);
    }
}
