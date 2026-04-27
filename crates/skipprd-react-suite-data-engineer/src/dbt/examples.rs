use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::OnceCell;
use tracing::{info, warn};

use react_core::agent::AgentCtx;
use react_core::provider_traits::{
    query_typed_documents, upsert_typed_documents, ScoredTypedVectorDocument, TypedVectorDocument,
    VectorCollection,
};

pub struct DbtExampleCollection;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DbtExampleMetadata {
    pub project: String,
    pub path: String,
    #[serde(default)]
    pub s3_uri: Option<String>,
}

impl VectorCollection for DbtExampleCollection {
    type Metadata = DbtExampleMetadata;

    const NAMESPACE: &'static str = "dbt_example";
}

pub type DbtExampleDocument = TypedVectorDocument<DbtExampleCollection>;
pub type ScoredDbtExampleDocument = ScoredTypedVectorDocument<DbtExampleCollection>;

static SYNC_ONCE: OnceCell<()> = OnceCell::const_new();

/// Ensure the local `dbt-examples/` folder is embedded once per process.
///
/// - Stores raw file bytes under `dbt-examples/<project>/<path>` (best effort).
/// - Upserts embeddings into the *current scope* vector store (best effort).
pub async fn ensure_synced_once(ctx: &AgentCtx) {
    let ctx = ctx.clone();
    let _ = SYNC_ONCE
        .get_or_init(|| async move {
            info!("dbt_examples: ensure_synced_once triggered (startup)");
            if let Err(e) = sync_from_local(&ctx).await {
                warn!("DBT examples sync skipped/failed: {}", e);
            }
        })
        .await;
}

pub async fn search_examples(
    ctx: &AgentCtx,
    query: &str,
    k: usize,
) -> Result<Vec<ScoredDbtExampleDocument>, String> {
    let vector = ctx.vector().as_ref().ok_or_else(|| {
        "vector provider missing (dbt examples search requires embeddings)".to_string()
    })?;
    let v = ctx
        .llm_embed(&[query.to_string()])
        .map_err(|e| e.to_string())?
        .pop()
        .unwrap_or_default();
    if v.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits =
        query_typed_documents::<DbtExampleCollection>(vector.as_ref(), ctx.scope(), &v, k * 3)
            .await?;
    hits.truncate(k.max(1));
    Ok(hits)
}

fn local_examples_root() -> PathBuf {
    PathBuf::from("dbt-examples")
}

fn include_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if name.starts_with('.') || name == "target" || name == "__pycache__" {
            return false;
        }
    }
    for c in path.components() {
        if let std::path::Component::Normal(os) = c {
            if let Some(s) = os.to_str() {
                let s_l = s.to_ascii_lowercase();
                if matches!(
                    s_l.as_str(),
                    ".git"
                        | ".github"
                        | ".circleci"
                        | ".vscode"
                        | ".idea"
                        | "node_modules"
                        | "venv"
                        | "__pycache__"
                        | "target"
                ) {
                    return false;
                }
            }
        }
    }
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "sql" | "yml" | "yaml" | "md" => true,
        _ => false,
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

async fn sync_from_local(ctx: &AgentCtx) -> Result<(), String> {
    let root = local_examples_root();
    if !root.exists() {
        return Err("dbt-examples/ folder not found".to_string());
    }

    let vector = ctx
        .vector()
        .as_ref()
        .ok_or_else(|| "vector provider missing".to_string())?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.clone()];
    while let Some(p) = stack.pop() {
        let rd = fs::read_dir(&p).map_err(|e| e.to_string())?;
        for ent in rd {
            let ent = ent.map_err(|e| e.to_string())?;
            let path = ent.path();
            if path.is_dir() {
                if include_dir(&path) {
                    stack.push(path);
                }
                continue;
            }
            if !include_file(&path) {
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            files.push((rel, bytes));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.is_empty() {
        return Ok(());
    }

    // Embed in small batches
    let batch = 48usize;
    let mut chunks: Vec<DbtExampleDocument> = Vec::with_capacity(files.len());
    for (rel, bytes) in files.iter() {
        let text = String::from_utf8_lossy(bytes).to_string();
        let project = rel.split('/').next().unwrap_or("dbt-examples").to_string();
        let id = format!("dbt_example:{}:{}", project, sha256_bytes(rel.as_bytes()));
        chunks.push(DbtExampleDocument::new(
            id,
            text,
            Vec::new(),
            chrono::Utc::now().timestamp() as u64,
            DbtExampleMetadata {
                project: project.clone(),
                path: rel.clone(),
                s3_uri: Some(format!("dbt-examples/{rel}")),
            },
        ));
    }

    let mut i = 0usize;
    while i < chunks.len() {
        let j = (i + batch).min(chunks.len());
        let texts: Vec<String> = chunks[i..j].iter().map(|c| c.text().to_string()).collect();
        let vecs = ctx.llm_embed(&texts).map_err(|e| e.to_string())?;
        for (k, v) in vecs.into_iter().enumerate() {
            let doc = &chunks[i + k];
            chunks[i + k] = DbtExampleDocument::new(
                doc.id().to_string(),
                doc.text().to_string(),
                v,
                doc.epoch(),
                doc.metadata().clone(),
            );
        }
        i = j;
    }

    // Best-effort store raw bytes in storage (so tool can point at an object key)
    for (rel, bytes) in files.into_iter() {
        let key = format!("dbt-examples/{}", rel);
        let _ = ctx
            .storage()
            .put_bytes(&key, &bytes, "application/octet-stream")
            .await;
    }

    // Upsert embeddings into current scope vector store
    let mut p = 0usize;
    while p < chunks.len() {
        let q = (p + batch).min(chunks.len());
        upsert_typed_documents(vector.as_ref(), ctx.scope(), &chunks[p..q]).await?;
        p = q;
    }
    Ok(())
}

fn include_dir(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        let n = name.to_ascii_lowercase();
        if matches!(
            n.as_str(),
            ".git" | "target" | "__pycache__" | "node_modules" | ".idea" | ".vscode"
        ) {
            return false;
        }
    }
    true
}
