use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use react_core::provider_traits::{delete_collection, upsert_typed_documents};
use react_core::suite::SuiteCtx;
use react_suite_debugger::repo_index::{AdminRepoCollection, AdminRepoDocument, AdminRepoMetadata};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_REPO_ROOTS: [&str; 2] = [
    "/Users/huders2000/Documents/sites/skippr/react",
    "/Users/huders2000/Documents/sites/skippr/skipprd",
];

const MAX_FILE_BYTES: usize = 512_000;
const CHUNK_CHARS: usize = 1400;
const EMBED_BATCH: usize = 48;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct RepoIndexManifest {
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default)]
    file_count: usize,
    #[serde(default)]
    chunk_count: usize,
    indexed_at: String,
    #[serde(default)]
    files: Vec<RepoIndexManifestFile>,
}

#[derive(Clone, Debug, Default)]
pub struct RepoIndexStatus {
    pub reused_existing_index: bool,
    pub file_count: usize,
    pub chunk_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct FileSnapshot {
    repo_root: String,
    repo_label: String,
    rel_path: String,
    sha256: String,
    text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RepoIndexManifestFile {
    repo_root: String,
    repo_label: String,
    rel_path: String,
    sha256: String,
    chunk_count: usize,
}

struct CollectedFiles {
    files: Vec<FileSnapshot>,
    available_roots: Vec<String>,
    warnings: Vec<String>,
}

pub async fn ensure_repo_indexed_with_progress<F>(
    ctx: &SuiteCtx,
    repo_roots: &[PathBuf],
    mut progress: F,
) -> Result<RepoIndexStatus, String>
where
    F: FnMut(String),
{
    let vector = ctx
        .vector()
        .as_ref()
        .ok_or_else(|| "vector provider missing".to_string())?;
    let manifest_key = manifest_key(ctx);
    let existing_manifest = match ctx.storage().get_json(&manifest_key).await {
        Ok(existing) => serde_json::from_value::<RepoIndexManifest>(existing).ok(),
        Err(_) => None,
    };
    progress(format!(
        "Scanning {} repo root(s) for admin debug context...",
        repo_roots.len()
    ));
    let CollectedFiles {
        files,
        available_roots,
        warnings,
    } = collect_files(repo_roots)?;
    if available_roots.is_empty() {
        let (file_count, chunk_count) = existing_manifest
            .as_ref()
            .map(manifest_totals)
            .unwrap_or((0, 0));
        progress(format!(
            "No local repo roots available; reusing current admin repo vectors ({} files / {} chunks).",
            file_count, chunk_count
        ));
        return Ok(RepoIndexStatus {
            reused_existing_index: true,
            file_count,
            chunk_count,
            warnings,
        });
    }

    let fingerprint = build_fingerprint(&files);
    if let Some(manifest) = existing_manifest.as_ref() {
        if manifest.files.is_empty() {
            if manifest.fingerprint == fingerprint {
                let (file_count, chunk_count) = manifest_totals(manifest);
                progress(format!(
                    "Repo index already up to date: {} files / {} chunks.",
                    file_count, chunk_count
                ));
                return Ok(RepoIndexStatus {
                    reused_existing_index: true,
                    file_count,
                    chunk_count,
                    warnings,
                });
            }
            if available_roots.len() != repo_roots.len() {
                let (file_count, chunk_count) = manifest_totals(manifest);
                progress(
                    "Some local repo roots are unavailable; reusing the current admin repo vectors."
                        .to_string(),
                );
                return Ok(RepoIndexStatus {
                    reused_existing_index: true,
                    file_count,
                    chunk_count,
                    warnings,
                });
            }
        } else {
            let available_root_set: HashSet<&str> =
                available_roots.iter().map(String::as_str).collect();
            let existing_by_key: HashMap<String, RepoIndexManifestFile> = manifest
                .files
                .iter()
                .cloned()
                .map(|file| (manifest_file_key(&file.repo_root, &file.rel_path), file))
                .collect();
            let current_keys: HashSet<String> = files.iter().map(file_snapshot_key).collect();

            let changed_files: Vec<FileSnapshot> = files
                .iter()
                .filter(|file| {
                    existing_by_key
                        .get(&file_snapshot_key(file))
                        .map(|existing| existing.sha256 != file.sha256)
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            let removed_files: Vec<RepoIndexManifestFile> = manifest
                .files
                .iter()
                .filter(|file| {
                    available_root_set.contains(file.repo_root.as_str())
                        && !current_keys
                            .contains(&manifest_file_key(&file.repo_root, &file.rel_path))
                })
                .cloned()
                .collect();

            if changed_files.is_empty() && removed_files.is_empty() {
                let (file_count, chunk_count) = manifest_totals(manifest);
                progress(format!(
                    "Repo index already up to date: {} files / {} chunks.",
                    file_count, chunk_count
                ));
                return Ok(RepoIndexStatus {
                    reused_existing_index: true,
                    file_count,
                    chunk_count,
                    warnings,
                });
            }

            progress(format!(
                "Updating admin repo context: {} changed/new file(s), {} removed file(s).",
                changed_files.len(),
                removed_files.len()
            ));

            if !removed_files.is_empty() {
                progress(format!(
                    "Removing vectors for {} deleted file(s)...",
                    removed_files.len()
                ));
                for file in &removed_files {
                    vector
                        .delete_ids_with_prefix(
                            ctx.scope(),
                            &file_vector_prefix(&file.repo_root, &file.rel_path),
                        )
                        .await?;
                }
            }

            if !changed_files.is_empty() {
                progress(format!(
                    "Refreshing vectors for {} changed/new file(s)...",
                    changed_files.len()
                ));
                for file in &changed_files {
                    vector
                        .delete_ids_with_prefix(
                            ctx.scope(),
                            &file_vector_prefix(&file.repo_root, &file.rel_path),
                        )
                        .await?;
                }
            }

            let (mut docs, changed_manifest_files) = build_documents(&changed_files);
            if !docs.is_empty() {
                let mut idx = 0usize;
                let total_batches = docs.len().div_ceil(EMBED_BATCH);
                let mut batch_num = 0usize;
                while idx < docs.len() {
                    let end = (idx + EMBED_BATCH).min(docs.len());
                    batch_num += 1;
                    progress(format!(
                        "Embedding batch {}/{} (chunks {}-{} of {})...",
                        batch_num,
                        total_batches,
                        idx + 1,
                        end,
                        docs.len()
                    ));
                    let texts: Vec<String> = docs[idx..end]
                        .iter()
                        .map(|d| d.text().to_string())
                        .collect();
                    let embeddings = ctx
                        .llm_embed(&texts)
                        .map_err(|e| format!("repo indexing embed failed: {e}"))?;
                    for (offset, embedding) in embeddings.into_iter().enumerate() {
                        let doc = &docs[idx + offset];
                        docs[idx + offset] = AdminRepoDocument::new(
                            doc.id().to_string(),
                            doc.text().to_string(),
                            embedding,
                            doc.epoch(),
                            doc.metadata().clone(),
                        );
                    }
                    upsert_typed_documents(vector.as_ref(), ctx.scope(), &docs[idx..end]).await?;
                    idx = end;
                }
            }

            let mut next_manifest_files: HashMap<String, RepoIndexManifestFile> = manifest
                .files
                .iter()
                .cloned()
                .map(|file| (manifest_file_key(&file.repo_root, &file.rel_path), file))
                .collect();
            for file in &removed_files {
                next_manifest_files.remove(&manifest_file_key(&file.repo_root, &file.rel_path));
            }
            for file in changed_manifest_files {
                next_manifest_files
                    .insert(manifest_file_key(&file.repo_root, &file.rel_path), file);
            }

            let mut next_manifest_files = next_manifest_files.into_values().collect::<Vec<_>>();
            sort_manifest_files(&mut next_manifest_files);
            progress("Persisting admin repo index manifest...".to_string());
            let manifest = build_manifest(&next_manifest_files);
            let manifest_json = serde_json::to_value(&manifest).map_err(|e| e.to_string())?;
            ctx.storage()
                .put_json(&manifest_key, &manifest_json)
                .await
                .map_err(|e| format!("failed to persist repo index manifest: {e}"))?;

            let (file_count, chunk_count) = manifest_totals(&manifest);
            return Ok(RepoIndexStatus {
                reused_existing_index: false,
                file_count,
                chunk_count,
                warnings,
            });
        }
    }

    let (mut docs, manifest_files) = build_documents(&files);

    progress(format!(
        "Reindexing admin repo context from {} file(s) into {} chunk(s)...",
        manifest_files.len(),
        docs.len()
    ));
    progress("Clearing previous admin repo index...".to_string());
    delete_collection::<AdminRepoCollection>(vector.as_ref(), ctx.scope()).await?;

    let mut idx = 0usize;
    let total_batches = docs.len().div_ceil(EMBED_BATCH);
    let mut batch_num = 0usize;
    while idx < docs.len() {
        let end = (idx + EMBED_BATCH).min(docs.len());
        batch_num += 1;
        progress(format!(
            "Embedding batch {}/{} (chunks {}-{} of {})...",
            batch_num,
            total_batches,
            idx + 1,
            end,
            docs.len()
        ));
        let texts: Vec<String> = docs[idx..end]
            .iter()
            .map(|d| d.text().to_string())
            .collect();
        let embeddings = ctx
            .llm_embed(&texts)
            .map_err(|e| format!("repo indexing embed failed: {e}"))?;
        for (offset, embedding) in embeddings.into_iter().enumerate() {
            let doc = &docs[idx + offset];
            docs[idx + offset] = AdminRepoDocument::new(
                doc.id().to_string(),
                doc.text().to_string(),
                embedding,
                doc.epoch(),
                doc.metadata().clone(),
            );
        }
        upsert_typed_documents(vector.as_ref(), ctx.scope(), &docs[idx..end]).await?;
        idx = end;
    }

    progress("Persisting admin repo index manifest...".to_string());
    let manifest = build_manifest(&manifest_files);
    let manifest_json = serde_json::to_value(&manifest).map_err(|e| e.to_string())?;
    ctx.storage()
        .put_json(&manifest_key, &manifest_json)
        .await
        .map_err(|e| format!("failed to persist repo index manifest: {e}"))?;

    let (file_count, chunk_count) = manifest_totals(&manifest);
    Ok(RepoIndexStatus {
        reused_existing_index: false,
        file_count,
        chunk_count,
        warnings,
    })
}

fn manifest_key(ctx: &SuiteCtx) -> String {
    let root = ctx
        .keyspace()
        .threads_prefix(ctx.scope())
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    format!("{root}/state/admin_repo_index_manifest.json")
}

fn collect_files(repo_roots: &[PathBuf]) -> Result<CollectedFiles, String> {
    let mut files = Vec::new();
    let mut available_roots = Vec::new();
    let mut warnings = Vec::new();

    for root in repo_roots {
        if !root.exists() {
            warnings.push(format!("repo root missing: {}", root.display()));
            continue;
        }
        if !root.is_dir() {
            warnings.push(format!("repo root is not a directory: {}", root.display()));
            continue;
        }

        let repo_root = root.to_string_lossy().to_string();
        available_roots.push(repo_root.clone());
        let repo_label = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();

        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| include_dir(e.path()))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if !include_file(entry.path()) {
                continue;
            }
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(entry.path()).map_err(|e| format!("read {rel_path}: {e}"))?;
            if bytes.len() > MAX_FILE_BYTES {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes).to_string();
            if text.trim().is_empty() {
                continue;
            }
            files.push(FileSnapshot {
                repo_root: repo_root.clone(),
                repo_label: repo_label.clone(),
                rel_path,
                sha256: sha256_bytes(&bytes),
                text,
            });
        }
    }

    files.sort_by(|a, b| {
        a.repo_root
            .cmp(&b.repo_root)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    available_roots.sort();
    available_roots.dedup();
    Ok(CollectedFiles {
        files,
        available_roots,
        warnings,
    })
}

fn build_documents(files: &[FileSnapshot]) -> (Vec<AdminRepoDocument>, Vec<RepoIndexManifestFile>) {
    let epoch = chrono::Utc::now().timestamp() as u64;
    let mut docs = Vec::new();
    let mut manifest_files = Vec::new();
    for file in files {
        let chunks = chunk_text(&file.text, CHUNK_CHARS);
        manifest_files.push(RepoIndexManifestFile {
            repo_root: file.repo_root.clone(),
            repo_label: file.repo_label.clone(),
            rel_path: file.rel_path.clone(),
            sha256: file.sha256.clone(),
            chunk_count: chunks.len(),
        });
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let text = format!(
                "repo_root: {}\nrepo: {}\npath: {}\n\n{}",
                file.repo_root, file.repo_label, file.rel_path, chunk
            );
            docs.push(AdminRepoDocument::new(
                format!(
                    "{}{}",
                    file_vector_prefix(&file.repo_root, &file.rel_path),
                    chunk_index
                ),
                text,
                Vec::new(),
                epoch,
                AdminRepoMetadata {
                    repo_root: file.repo_root.clone(),
                    repo_label: file.repo_label.clone(),
                    path: file.rel_path.clone(),
                    chunk_index,
                    sha256: file.sha256.clone(),
                },
            ));
        }
    }
    (docs, manifest_files)
}

fn chunk_text(text: &str, chunk_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if para.chars().count() > chunk_chars {
            if !buf.trim().is_empty() {
                out.push(buf.trim().to_string());
                buf.clear();
            }
            let chars: Vec<char> = para.chars().collect();
            let mut start = 0usize;
            while start < chars.len() {
                let end = (start + chunk_chars).min(chars.len());
                out.push(chars[start..end].iter().collect());
                start = end;
            }
            continue;
        }
        if !buf.is_empty() && buf.len() + para.len() + 2 > chunk_chars {
            out.push(buf.trim().to_string());
            buf.clear();
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(para);
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    if out.is_empty() && !text.trim().is_empty() {
        let chars: Vec<char> = text.chars().collect();
        let mut start = 0usize;
        while start < chars.len() {
            let end = (start + chunk_chars).min(chars.len());
            out.push(chars[start..end].iter().collect());
            start = end;
        }
    }
    out
}

fn build_fingerprint(files: &[FileSnapshot]) -> String {
    let mut h = Sha256::new();
    for file in files {
        h.update(file.repo_root.as_bytes());
        h.update([0]);
        h.update(file.rel_path.as_bytes());
        h.update([0]);
        h.update(file.sha256.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())
}

fn manifest_totals(manifest: &RepoIndexManifest) -> (usize, usize) {
    if manifest.files.is_empty() {
        (manifest.file_count, manifest.chunk_count)
    } else {
        (
            manifest.files.len(),
            manifest.files.iter().map(|file| file.chunk_count).sum(),
        )
    }
}

fn build_manifest(files: &[RepoIndexManifestFile]) -> RepoIndexManifest {
    RepoIndexManifest {
        fingerprint: build_manifest_fingerprint(files),
        roots: manifest_roots(files),
        file_count: files.len(),
        chunk_count: files.iter().map(|file| file.chunk_count).sum(),
        indexed_at: chrono::Utc::now().to_rfc3339(),
        files: files.to_vec(),
    }
}

fn build_manifest_fingerprint(files: &[RepoIndexManifestFile]) -> String {
    let mut h = Sha256::new();
    for file in files {
        h.update(file.repo_root.as_bytes());
        h.update([0]);
        h.update(file.rel_path.as_bytes());
        h.update([0]);
        h.update(file.sha256.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())
}

fn manifest_roots(files: &[RepoIndexManifestFile]) -> Vec<String> {
    let mut roots = files
        .iter()
        .map(|file| file.repo_root.clone())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

fn sort_manifest_files(files: &mut [RepoIndexManifestFile]) {
    files.sort_by(|a, b| {
        a.repo_root
            .cmp(&b.repo_root)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
}

fn file_snapshot_key(file: &FileSnapshot) -> String {
    manifest_file_key(&file.repo_root, &file.rel_path)
}

fn manifest_file_key(repo_root: &str, rel_path: &str) -> String {
    format!("{repo_root}\u{0}{rel_path}")
}

fn file_vector_prefix(repo_root: &str, rel_path: &str) -> String {
    format!(
        "admin_repo:{}:",
        sha256_bytes(manifest_file_key(repo_root, rel_path).as_bytes())
    )
}

fn include_dir(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            ".git"
                | ".cursor"
                | ".idea"
                | ".vscode"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | ".next"
        ) {
            return false;
        }
    }
    true
}

fn include_file(path: &Path) -> bool {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" | "toml" | "lock" | "md" | "mdx" | "txt" | "yml" | "yaml" | "json" | "js" | "jsx"
        | "ts" | "tsx" | "sql" | "sh" | "proto" | "graphql" | "gql" | "css" | "html" => true,
        _ => false,
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::error::CoreError;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::llm::{ChatMessage, LargeLanguageModel, LlmCallOptions, NullModel};
    use react_core::provider_traits::NullSecretsProvider;
    use react_core::provider_traits::{StoredVectorRecord, VectorStore};
    use react_core::storage::{ConditionalWriteStatus, StorageAdapter};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn chunk_text_splits_large_inputs() {
        let text = "a".repeat(2000);
        let chunks = chunk_text(&text, 700);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn include_file_filters_expected_extensions() {
        assert!(include_file(Path::new("src/main.rs")));
        assert!(include_file(Path::new("docs/guide.md")));
        assert!(!include_file(Path::new("assets/logo.png")));
    }

    #[derive(Default)]
    struct MockStorage {
        json: Mutex<HashMap<String, serde_json::Value>>,
    }

    #[async_trait]
    impl StorageAdapter for MockStorage {
        async fn get_json(&self, key: &str) -> Result<serde_json::Value, CoreError> {
            self.json
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| CoreError::generic(format!("missing key: {key}")))
        }

        async fn put_json(&self, key: &str, value: &serde_json::Value) -> Result<(), CoreError> {
            self.json
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn put_json_if_etag_matches(
            &self,
            key: &str,
            value: &serde_json::Value,
            _expected_etag: Option<&str>,
        ) -> Result<ConditionalWriteStatus, CoreError> {
            self.json
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(ConditionalWriteStatus::Written)
        }

        async fn get_bytes(&self, _key: &str) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::generic("not implemented"))
        }

        async fn put_bytes(
            &self,
            _key: &str,
            _bytes: &[u8],
            _content_type: &str,
        ) -> Result<(), CoreError> {
            Ok(())
        }

        async fn delete_object(&self, _key: &str) -> Result<(), CoreError> {
            Ok(())
        }

        async fn head_etag(&self, _key: &str) -> Result<Option<String>, CoreError> {
            Ok(None)
        }

        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>, CoreError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct MockVectorStore {
        deleted_namespaces: Mutex<Vec<String>>,
        deleted_id_prefixes: Mutex<Vec<String>>,
        upsert_batches: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl VectorStore for MockVectorStore {
        async fn upsert(
            &self,
            _scope: &react_core::scope::RequestScope,
            items: &[StoredVectorRecord],
        ) -> Result<(), String> {
            self.upsert_batches.lock().unwrap().push(items.len());
            Ok(())
        }

        async fn query(
            &self,
            _scope: &react_core::scope::RequestScope,
            _query_vec: &[f32],
            _k: usize,
            _namespace: Option<&str>,
        ) -> Result<Vec<react_core::provider_traits::ScoredVectorRecord>, String> {
            Ok(Vec::new())
        }

        async fn delete_thread_embeddings(
            &self,
            _scope: &react_core::scope::RequestScope,
            _thread_id: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn delete_project_embeddings(
            &self,
            _scope: &react_core::scope::RequestScope,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn delete_namespace(
            &self,
            _scope: &react_core::scope::RequestScope,
            namespace: &str,
        ) -> Result<(), String> {
            self.deleted_namespaces
                .lock()
                .unwrap()
                .push(namespace.to_string());
            Ok(())
        }

        async fn delete_ids_with_prefix(
            &self,
            _scope: &react_core::scope::RequestScope,
            prefix: &str,
        ) -> Result<(), String> {
            self.deleted_id_prefixes
                .lock()
                .unwrap()
                .push(prefix.to_string());
            Ok(())
        }
    }

    struct MockLlm;

    impl LargeLanguageModel for MockLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &LlmCallOptions,
        ) -> Result<String, String> {
            NullModel::new().chat(_messages, _options)
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|t| vec![t.len() as f32]).collect())
        }
    }

    #[derive(Default)]
    struct CountingMockLlm {
        embed_batches: Mutex<Vec<usize>>,
    }

    impl LargeLanguageModel for CountingMockLlm {
        fn chat(
            &self,
            messages: &[ChatMessage],
            options: &LlmCallOptions,
        ) -> Result<String, String> {
            NullModel::new().chat(messages, options)
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.embed_batches.lock().unwrap().push(texts.len());
            Ok(texts.iter().map(|t| vec![t.len() as f32]).collect())
        }
    }

    #[tokio::test]
    async fn ensure_repo_indexed_reuses_manifest_when_inputs_do_not_change() {
        let root =
            std::env::temp_dir().join(format!("skippr-admin-index-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .expect("write");

        let storage: Arc<dyn StorageAdapter> = Arc::new(MockStorage::default());
        let vector = Arc::new(MockVectorStore::default());
        let scope = react_core::scope::RequestScope::parse("tenant", "skippr-admin", "ws--proj")
            .expect("scope");
        let keyspace = Arc::new(DefaultKeyspace::new(String::new()));
        let mut sctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider),
            Arc::new(MockLlm),
            scope,
            keyspace,
        );
        sctx.set_vector(Some(vector.clone()));

        let progress = Arc::new(Mutex::new(Vec::<String>::new()));
        let progress_first = progress.clone();
        let first = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], move |msg| {
            progress_first.lock().unwrap().push(msg);
        })
        .await
        .expect("first");
        let progress_second = progress.clone();
        let second = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], move |msg| {
            progress_second.lock().unwrap().push(msg);
        })
        .await
        .expect("second");

        assert!(!first.reused_existing_index);
        assert!(first.chunk_count > 0);
        assert!(second.reused_existing_index);
        assert_eq!(vector.deleted_namespaces.lock().unwrap().len(), 1);
        assert_eq!(vector.upsert_batches.lock().unwrap().len(), 1);
        let progress_messages = progress.lock().unwrap();
        assert!(progress_messages
            .iter()
            .any(|msg| msg.contains("Reindexing admin repo context")));
        assert!(progress_messages
            .iter()
            .any(|msg| msg.contains("Embedding batch")));
        assert!(progress_messages
            .iter()
            .any(|msg| msg.contains("Repo index already up to date")));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_repo_indexed_only_updates_changed_files() {
        let root =
            std::env::temp_dir().join(format!("skippr-admin-delta-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/a.rs"), "fn a() { println!(\"a1\"); }\n").expect("write a");
        fs::write(root.join("src/b.rs"), "fn b() { println!(\"b1\"); }\n").expect("write b");

        let storage: Arc<dyn StorageAdapter> = Arc::new(MockStorage::default());
        let vector = Arc::new(MockVectorStore::default());
        let llm = Arc::new(CountingMockLlm::default());
        let scope = react_core::scope::RequestScope::parse("tenant", "skippr-admin", "ws--proj")
            .expect("scope");
        let keyspace = Arc::new(DefaultKeyspace::new(String::new()));
        let mut sctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider),
            llm.clone(),
            scope,
            keyspace,
        );
        sctx.set_vector(Some(vector.clone()));

        let first = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], |_| {})
            .await
            .expect("first");
        fs::write(root.join("src/a.rs"), "fn a() { println!(\"a2\"); }\n").expect("rewrite a");
        let second = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], |_| {})
            .await
            .expect("second");

        assert!(!first.reused_existing_index);
        assert!(!second.reused_existing_index);
        assert_eq!(vector.deleted_namespaces.lock().unwrap().len(), 1);
        assert_eq!(vector.deleted_id_prefixes.lock().unwrap().len(), 1);
        assert_eq!(*llm.embed_batches.lock().unwrap(), vec![2, 1]);
        assert_eq!(*vector.upsert_batches.lock().unwrap(), vec![2, 1]);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_repo_indexed_reuses_existing_vectors_when_repo_roots_missing() {
        let root = std::env::temp_dir().join(format!(
            "skippr-admin-missing-root-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .expect("write");

        let storage: Arc<dyn StorageAdapter> = Arc::new(MockStorage::default());
        let vector = Arc::new(MockVectorStore::default());
        let scope = react_core::scope::RequestScope::parse("tenant", "skippr-admin", "ws--proj")
            .expect("scope");
        let keyspace = Arc::new(DefaultKeyspace::new(String::new()));
        let mut sctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider),
            Arc::new(MockLlm),
            scope,
            keyspace,
        );
        sctx.set_vector(Some(vector.clone()));

        let first = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], |_| {})
            .await
            .expect("first");
        fs::remove_dir_all(&root).expect("remove root");
        let second = ensure_repo_indexed_with_progress(&sctx, &[root.clone()], |_| {})
            .await
            .expect("second");

        assert!(!first.reused_existing_index);
        assert!(second.reused_existing_index);
        assert_eq!(second.file_count, first.file_count);
        assert_eq!(second.chunk_count, first.chunk_count);
        assert_eq!(vector.deleted_namespaces.lock().unwrap().len(), 1);
        assert!(vector.deleted_id_prefixes.lock().unwrap().is_empty());
        assert_eq!(*vector.upsert_batches.lock().unwrap(), vec![1]);
        assert_eq!(
            second.warnings,
            vec![format!("repo root missing: {}", root.display())]
        );
    }
}
